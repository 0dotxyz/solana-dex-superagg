//! Transaction builder for Titan swap routes.
//!
//! Titan hands back bare route instructions, so everything around them is ours to add: the compute
//! budget (Titan's routes carry none — without it a route runs under the 200k default limit with no
//! priority fee) and, for a native-SOL input, the wrap into the wSOL ATA.

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta as SolanaAccountMeta, Instruction as SolanaInstruction},
    message::{v0, AddressLookupTableAccount, VersionedMessage},
    pubkey::Pubkey as SolanaPubkey,
};

use super::types::{AccountMeta, Instruction, SwapRoute};
use crate::aggregators::sol;

/// CU limit when Titan doesn't recommend one: the per-transaction maximum, so the route can't run
/// out of compute (at the cost of a larger priority fee for the same CU price).
const FALLBACK_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// Lookup-table account layout: 56 bytes of metadata, then 32-byte addresses.
const LOOKUP_TABLE_META_SIZE: usize = 56;

fn titan_account_meta_to_solana(meta: &AccountMeta) -> SolanaAccountMeta {
    SolanaAccountMeta {
        pubkey: SolanaPubkey::new_from_array(meta.p),
        is_signer: meta.s,
        is_writable: meta.w,
    }
}

fn titan_instruction_to_solana(instruction: &Instruction) -> SolanaInstruction {
    SolanaInstruction {
        program_id: SolanaPubkey::new_from_array(instruction.p),
        accounts: instruction
            .a
            .iter()
            .map(titan_account_meta_to_solana)
            .collect(),
        data: instruction.d.clone(),
    }
}

/// The CU limit to request for `route`.
fn compute_unit_limit(route: &SwapRoute) -> u32 {
    route
        .compute_units_safe
        .or(route.compute_units.map(|units| units.saturating_mul(6) / 5))
        .map(|units| units.min(FALLBACK_COMPUTE_UNIT_LIMIT as u64) as u32)
        .unwrap_or(FALLBACK_COMPUTE_UNIT_LIMIT)
}

/// Everything the transaction executes, in order: compute budget, optional SOL wrap, the route.
pub(crate) fn route_instructions(
    route: &SwapRoute,
    payer: &SolanaPubkey,
    compute_unit_price_micro_lamports: u64,
    wrap_input_lamports: Option<u64>,
) -> Vec<SolanaInstruction> {
    let mut instructions = vec![sol::set_compute_unit_limit(compute_unit_limit(route))];
    if compute_unit_price_micro_lamports > 0 {
        instructions.push(sol::set_compute_unit_price(
            compute_unit_price_micro_lamports,
        ));
    }
    if let Some(lamports) = wrap_input_lamports {
        instructions.extend(sol::wrap_sol_instructions(payer, lamports));
    }
    instructions.extend(route.instructions.iter().map(titan_instruction_to_solana));
    instructions
}

fn parse_lookup_table_addresses(data: &[u8]) -> Result<Vec<SolanaPubkey>> {
    let addresses = data
        .get(LOOKUP_TABLE_META_SIZE..)
        .ok_or_else(|| anyhow!("Lookup table data too small"))?;
    Ok(addresses
        .chunks_exact(32)
        .map(|chunk| SolanaPubkey::new_from_array(chunk.try_into().unwrap()))
        .collect())
}

/// Fetch every lookup table the route needs. A missing table is an error: compiling without it
/// would silently produce a transaction that references accounts it can't resolve.
pub(crate) async fn fetch_lookup_tables(
    rpc: &RpcClient,
    route: &SwapRoute,
) -> Result<Vec<AddressLookupTableAccount>> {
    if route.address_lookup_tables.is_empty() {
        return Ok(vec![]);
    }
    let keys: Vec<SolanaPubkey> = route
        .address_lookup_tables
        .iter()
        .map(|key| SolanaPubkey::new_from_array(*key))
        .collect();
    let accounts = rpc
        .get_multiple_accounts(&keys)
        .await
        .map_err(|e| anyhow!("Failed to fetch Titan lookup tables: {}", e))?;

    keys.into_iter()
        .zip(accounts)
        .map(|(key, account)| {
            let account = account.ok_or_else(|| anyhow!("Lookup table {} not found", key))?;
            Ok(AddressLookupTableAccount {
                key,
                addresses: parse_lookup_table_addresses(&account.data)?,
            })
        })
        .collect()
}

/// Compile `instructions` into a v0 message.
pub(crate) fn compile_message(
    payer: &SolanaPubkey,
    instructions: &[SolanaInstruction],
    lookup_tables: &[AddressLookupTableAccount],
    recent_blockhash: Hash,
) -> Result<VersionedMessage> {
    let message = v0::Message::try_compile(payer, instructions, lookup_tables, recent_blockhash)
        .map_err(|e| anyhow!("Failed to compile V0 message: {}", e))?;
    Ok(VersionedMessage::V0(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(compute_units: Option<u64>, compute_units_safe: Option<u64>) -> SwapRoute {
        SwapRoute {
            in_amount: 1,
            out_amount: 1,
            slippage_bps: 50,
            platform_fee: None,
            steps: vec![],
            instructions: vec![Instruction {
                p: [9; 32],
                a: vec![AccountMeta {
                    p: [8; 32],
                    s: false,
                    w: true,
                }],
                d: vec![42],
            }],
            address_lookup_tables: vec![],
            context_slot: None,
            time_taken_ns: None,
            expires_at_ms: None,
            expires_after_slot: None,
            compute_units,
            compute_units_safe,
            transaction: None,
            reference_id: None,
        }
    }

    #[test]
    fn compute_unit_limit_prefers_the_safe_recommendation() {
        assert_eq!(
            compute_unit_limit(&route(Some(100_000), Some(250_000))),
            250_000
        );
        assert_eq!(compute_unit_limit(&route(Some(100_000), None)), 120_000);
        assert_eq!(compute_unit_limit(&route(None, None)), 1_400_000);
        assert_eq!(compute_unit_limit(&route(None, Some(9_000_000))), 1_400_000);
    }

    #[test]
    fn route_is_preceded_by_compute_budget_and_wrap() {
        let payer = SolanaPubkey::new_unique();
        let ixs = route_instructions(&route(None, Some(300_000)), &payer, 5_000, Some(77));

        // limit, price, create ATA, transfer, sync, route
        assert_eq!(ixs.len(), 6);
        assert_eq!(ixs[0], sol::set_compute_unit_limit(300_000));
        assert_eq!(ixs[1], sol::set_compute_unit_price(5_000));
        assert_eq!(
            &ixs[2..5],
            sol::wrap_sol_instructions(&payer, 77).as_slice()
        );
        assert_eq!(ixs[5].program_id, SolanaPubkey::new_from_array([9; 32]));
        assert_eq!(ixs[5].data, vec![42]);
    }

    #[test]
    fn no_price_or_wrap_unless_asked() {
        let payer = SolanaPubkey::new_unique();
        let ixs = route_instructions(&route(None, Some(300_000)), &payer, 0, None);
        assert_eq!(ixs.len(), 2);
        assert_eq!(ixs[0], sol::set_compute_unit_limit(300_000));
    }

    #[test]
    fn lookup_table_addresses_follow_the_metadata() {
        let mut data = vec![0u8; LOOKUP_TABLE_META_SIZE];
        data.extend_from_slice(&[1u8; 32]);
        data.extend_from_slice(&[2u8; 32]);
        let addresses = parse_lookup_table_addresses(&data).unwrap();
        assert_eq!(
            addresses,
            vec![
                SolanaPubkey::new_from_array([1; 32]),
                SolanaPubkey::new_from_array([2; 32])
            ]
        );
        assert!(parse_lookup_table_addresses(&[0u8; 10]).is_err());
    }
}

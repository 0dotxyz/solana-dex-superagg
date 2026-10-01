//! Turns a Titan route into transaction instructions. Titan routes carry no compute budget (without
//! one a route runs under the 200k default limit with no priority fee), so we add it, plus the wrap
//! of a native-SOL input.

use anyhow::{anyhow, Result};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::AddressLookupTableAccount,
    pubkey::Pubkey,
};
use solana_system_interface::instruction::transfer;
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account_idempotent,
};
use spl_token_interface::{instruction::sync_native, native_mint};

use super::types::{self, SwapRoute};

/// CU limit when Titan doesn't recommend one: the per-transaction maximum.
const FALLBACK_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

fn to_solana_instruction(instruction: &types::Instruction) -> Instruction {
    Instruction {
        program_id: Pubkey::new_from_array(instruction.p),
        accounts: instruction
            .a
            .iter()
            .map(|meta| AccountMeta {
                pubkey: Pubkey::new_from_array(meta.p),
                is_signer: meta.s,
                is_writable: meta.w,
            })
            .collect(),
        data: instruction.d.clone(),
    }
}

fn compute_unit_limit(route: &SwapRoute) -> u32 {
    route
        .compute_units_safe
        .or(route.compute_units.map(|units| units.saturating_mul(6) / 5))
        .map(|units| units.min(FALLBACK_COMPUTE_UNIT_LIMIT as u64) as u32)
        .unwrap_or(FALLBACK_COMPUTE_UNIT_LIMIT)
}

/// Compute budget, then the optional wrap of `wrap_input_lamports` into the wSOL ATA, then the route.
pub(crate) fn route_instructions(
    route: &SwapRoute,
    payer: &Pubkey,
    compute_unit_price_micro_lamports: u64,
    wrap_input_lamports: Option<u64>,
) -> Result<Vec<Instruction>> {
    let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_limit(
        compute_unit_limit(route),
    )];
    if compute_unit_price_micro_lamports > 0 {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
            compute_unit_price_micro_lamports,
        ));
    }
    if let Some(lamports) = wrap_input_lamports {
        let wsol_ata = get_associated_token_address(payer, &native_mint::ID);
        instructions.extend([
            create_associated_token_account_idempotent(
                payer,
                payer,
                &native_mint::ID,
                &spl_token_interface::ID,
            ),
            transfer(payer, &wsol_ata, lamports),
            sync_native(&spl_token_interface::ID, &wsol_ata)?,
        ]);
    }
    instructions.extend(route.instructions.iter().map(to_solana_instruction));
    Ok(instructions)
}

/// Fetch every lookup table the route needs; a missing one is an error rather than a transaction
/// that references accounts it can't resolve.
pub(crate) async fn fetch_lookup_tables(
    rpc: &RpcClient,
    route: &SwapRoute,
) -> Result<Vec<AddressLookupTableAccount>> {
    if route.address_lookup_tables.is_empty() {
        return Ok(vec![]);
    }
    let keys: Vec<Pubkey> = route
        .address_lookup_tables
        .iter()
        .map(|key| Pubkey::new_from_array(*key))
        .collect();
    let accounts = rpc
        .get_multiple_accounts(&keys)
        .await
        .map_err(|e| anyhow!("Failed to fetch Titan lookup tables: {}", e))?;

    keys.into_iter()
        .zip(accounts)
        .map(|(key, account)| {
            let account = account.ok_or_else(|| anyhow!("Lookup table {} not found", key))?;
            let table = AddressLookupTable::deserialize(&account.data)
                .map_err(|e| anyhow!("Invalid lookup table {}: {}", key, e))?;
            Ok(AddressLookupTableAccount {
                key,
                addresses: table.addresses.to_vec(),
            })
        })
        .collect()
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
            instructions: vec![types::Instruction {
                p: [9; 32],
                a: vec![],
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
        let payer = Pubkey::new_unique();
        let ixs = route_instructions(&route(None, Some(300_000)), &payer, 5_000, Some(77)).unwrap();

        // limit, price, create ATA, transfer, sync, route
        assert_eq!(ixs.len(), 6);
        assert_eq!(
            ixs[0],
            ComputeBudgetInstruction::set_compute_unit_limit(300_000)
        );
        assert_eq!(
            ixs[1],
            ComputeBudgetInstruction::set_compute_unit_price(5_000)
        );
        let wsol_ata = get_associated_token_address(&payer, &native_mint::ID);
        assert_eq!(ixs[3], transfer(&payer, &wsol_ata, 77));
        assert_eq!(
            ixs[5],
            to_solana_instruction(&route(None, None).instructions[0])
        );

        let plain = route_instructions(&route(None, Some(300_000)), &payer, 0, None).unwrap();
        assert_eq!(plain.len(), 2);
    }
}

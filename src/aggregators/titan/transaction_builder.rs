//! Titan routes carry no compute budget: without one a route runs under the 200k default limit
//! with no priority fee, so we add it.

use anyhow::{anyhow, Result};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_message::AddressLookupTableAccount;
use solana_pubkey::Pubkey;

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

pub(crate) fn solana_instructions(route: &SwapRoute) -> impl Iterator<Item = Instruction> + '_ {
    route.instructions.iter().map(to_solana_instruction)
}

pub(crate) fn lookup_table_addresses(route: &SwapRoute) -> Vec<Pubkey> {
    route
        .address_lookup_tables
        .iter()
        .map(|key| Pubkey::new_from_array(*key))
        .collect()
}

fn compute_unit_limit(route: &SwapRoute) -> u32 {
    route
        .compute_units_safe
        .or(route.compute_units.map(|units| units.saturating_mul(6) / 5))
        .map(|units| units.min(FALLBACK_COMPUTE_UNIT_LIMIT as u64) as u32)
        .unwrap_or(FALLBACK_COMPUTE_UNIT_LIMIT)
}

/// The compute budget followed by the route.
pub(crate) fn transaction_instructions(
    route: &SwapRoute,
    compute_unit_price_micro_lamports: u64,
) -> Vec<Instruction> {
    let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_limit(
        compute_unit_limit(route),
    )];
    if compute_unit_price_micro_lamports > 0 {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
            compute_unit_price_micro_lamports,
        ));
    }
    instructions.extend(solana_instructions(route));
    instructions
}

/// Every lookup table the route needs; a missing one is an error rather than a transaction that
/// references accounts it can't resolve.
pub(crate) async fn fetch_lookup_tables(
    rpc: &RpcClient,
    route: &SwapRoute,
) -> Result<Vec<AddressLookupTableAccount>> {
    if route.address_lookup_tables.is_empty() {
        return Ok(vec![]);
    }
    let keys = lookup_table_addresses(route);
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
            out_amount: 1,
            slippage_bps: None,
            instructions: vec![types::Instruction {
                p: [9; 32],
                a: vec![],
                d: vec![42],
            }],
            address_lookup_tables: vec![],
            expires_at_ms: None,
            compute_units,
            compute_units_safe,
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
    fn route_is_preceded_by_the_compute_budget() {
        let ixs = transaction_instructions(&route(None, Some(300_000)), 5_000);
        assert_eq!(ixs.len(), 3);
        assert_eq!(
            ixs[0],
            ComputeBudgetInstruction::set_compute_unit_limit(300_000)
        );
        assert_eq!(
            ixs[1],
            ComputeBudgetInstruction::set_compute_unit_price(5_000)
        );
        assert_eq!(
            ixs[2],
            to_solana_instruction(&route(None, None).instructions[0])
        );

        assert_eq!(
            transaction_instructions(&route(None, Some(300_000)), 0).len(),
            2
        );
    }
}

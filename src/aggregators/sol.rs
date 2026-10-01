//! Native-SOL and compute-budget instruction helpers for venues that hand back raw instructions.
//!
//! The instructions are encoded by hand (they are tiny, stable wire formats) so we don't pull in
//! extra interface crates whose versions would have to track solana-sdk.

use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};

pub const WSOL_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");

/// `ComputeBudgetInstruction::SetComputeUnitLimit`.
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: vec![],
        data,
    }
}

/// `ComputeBudgetInstruction::SetComputeUnitPrice`.
pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: vec![],
        data,
    }
}

/// The wallet's wSOL associated token account.
pub fn wsol_ata(owner: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(owner, &WSOL_MINT, &TOKEN_PROGRAM_ID)
}

/// Wrap `lamports` of the owner's native SOL into its wSOL ATA: create the ATA if needed, move the
/// lamports in, and sync the token balance.
pub fn wrap_sol_instructions(owner: &Pubkey, lamports: u64) -> Vec<Instruction> {
    let ata = wsol_ata(owner);

    let mut transfer_data = 2u32.to_le_bytes().to_vec();
    transfer_data.extend_from_slice(&lamports.to_le_bytes());
    let transfer = Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*owner, true), AccountMeta::new(ata, false)],
        data: transfer_data,
    };

    let sync_native = Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(ata, false)],
        data: vec![17u8],
    };

    vec![
        create_associated_token_account_idempotent(owner, owner, &WSOL_MINT, &TOKEN_PROGRAM_ID),
        transfer,
        sync_native,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_budget_encoding() {
        let limit = set_compute_unit_limit(300_000);
        assert_eq!(limit.program_id, COMPUTE_BUDGET_PROGRAM_ID);
        assert_eq!(limit.data, [2, 0xe0, 0x93, 0x04, 0x00]);

        let price = set_compute_unit_price(1_000);
        assert_eq!(price.data, [3, 0xe8, 0x03, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn wrap_sol_moves_lamports_into_the_wsol_ata() {
        let owner = Pubkey::new_unique();
        let ata = wsol_ata(&owner);
        let ixs = wrap_sol_instructions(&owner, 5);

        assert_eq!(ixs.len(), 3);
        assert_eq!(ixs[0].program_id, spl_associated_token_account::id());

        let transfer = &ixs[1];
        assert_eq!(transfer.program_id, SYSTEM_PROGRAM_ID);
        assert_eq!(transfer.data, [2, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(transfer.accounts[0], AccountMeta::new(owner, true));
        assert_eq!(transfer.accounts[1], AccountMeta::new(ata, false));

        let sync = &ixs[2];
        assert_eq!(sync.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(sync.data, [17]);
        assert_eq!(sync.accounts, vec![AccountMeta::new(ata, false)]);
    }
}

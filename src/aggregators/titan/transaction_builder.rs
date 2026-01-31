//! Transaction builder for Titan swap routes
//! Converts Titan's instruction format to Solana VersionedTransaction

use reqwest::Client;
use serde_json::{json, Value};
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta as SolanaAccountMeta, Instruction as SolanaInstruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey as SolanaPubkey,
    signature::Signature as SolanaSignature,
    transaction::VersionedTransaction,
};
use std::str::FromStr;

use super::types::{AccountMeta, Instruction, Pubkey, SwapRoute};

/// Convert Titan's 32-byte pubkey to Solana Pubkey
fn titan_pubkey_to_solana(pubkey: &Pubkey) -> Result<SolanaPubkey, String> {
    SolanaPubkey::try_from(&pubkey[..]).map_err(|e| format!("Invalid pubkey: {}", e))
}

/// Convert Titan's AccountMeta to Solana AccountMeta
fn titan_account_meta_to_solana(meta: &AccountMeta) -> Result<SolanaAccountMeta, String> {
    let pubkey = titan_pubkey_to_solana(&meta.p)?;
    Ok(SolanaAccountMeta {
        pubkey,
        is_signer: meta.s,
        is_writable: meta.w,
    })
}

/// Convert Titan's Instruction to Solana Instruction
fn titan_instruction_to_solana(instruction: &Instruction) -> Result<SolanaInstruction, String> {
    let program_id = titan_pubkey_to_solana(&instruction.p)?;

    let accounts: Result<Vec<SolanaAccountMeta>, String> = instruction
        .a
        .iter()
        .map(titan_account_meta_to_solana)
        .collect();

    Ok(SolanaInstruction {
        program_id,
        accounts: accounts?,
        data: instruction.d.clone(),
    })
}

/// Fetch address lookup table account data from RPC
async fn fetch_lookup_table_accounts(
    lookup_table_pubkeys: &[SolanaPubkey],
    rpc_url: &str,
) -> Result<Vec<AddressLookupTableAccount>, String> {
    let client = Client::new();
    let mut lookup_table_accounts = Vec::new();

    for pubkey in lookup_table_pubkeys {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [
                pubkey.to_string(),
                {
                    "encoding": "base64"
                }
            ]
        });

        let response = client
            .post(rpc_url)
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Failed to fetch lookup table: {}", e))?;

        let json: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse lookup table response: {}", e))?;

        // Extract account data
        if let Some(data_array) = json["result"]["value"]["data"].as_array() {
            if let Some(data_str) = data_array.first().and_then(|v| v.as_str()) {
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data_str)
                    .map_err(|e| format!("Failed to decode lookup table data: {}", e))?;

                // Parse the lookup table account
                let lookup_table = AddressLookupTableAccount {
                    key: *pubkey,
                    addresses: parse_lookup_table_addresses(&data)?,
                };

                lookup_table_accounts.push(lookup_table);
            }
        }
    }

    Ok(lookup_table_accounts)
}

/// Parse addresses from lookup table account data
fn parse_lookup_table_addresses(data: &[u8]) -> Result<Vec<SolanaPubkey>, String> {
    // Lookup table format: discriminator (8) + meta (56) + addresses (32 each)
    const LOOKUP_TABLE_META_SIZE: usize = 56;

    if data.len() < LOOKUP_TABLE_META_SIZE {
        return Err("Lookup table data too small".to_string());
    }

    let addresses_data = &data[LOOKUP_TABLE_META_SIZE..];
    let num_addresses = addresses_data.len() / 32;

    let mut addresses = Vec::with_capacity(num_addresses);
    for i in 0..num_addresses {
        let start = i * 32;
        let end = start + 32;
        let address_bytes: [u8; 32] = addresses_data[start..end]
            .try_into()
            .map_err(|_| "Invalid address bytes".to_string())?;
        addresses.push(
            SolanaPubkey::try_from(&address_bytes[..])
                .map_err(|_| "Invalid pubkey bytes".to_string())?,
        );
    }

    Ok(addresses)
}

/// Build a VersionedTransaction from Titan's SwapRoute
///
/// # Arguments
/// * `route` - The Titan swap route containing instructions and lookup tables
/// * `payer` - The transaction fee payer pubkey
/// * `recent_blockhash` - Recent blockhash for the transaction
/// * `rpc_url` - RPC endpoint to fetch lookup table accounts
/// * `jito_tip_lamports` - Optional Jito tip amount in lamports
///
/// # Returns
/// Serialized transaction bytes ready for signing
pub async fn build_transaction_from_route(
    route: &SwapRoute,
    payer: SolanaPubkey,
    recent_blockhash: Hash,
    rpc_url: &str,
    jito_tip_lamports: Option<u64>,
) -> Result<Vec<u8>, String> {
    // Convert Titan instructions to Solana instructions
    let instructions: Result<Vec<SolanaInstruction>, String> = route
        .instructions
        .iter()
        .map(titan_instruction_to_solana)
        .collect();

    let mut instructions = instructions?;

    // Add Jito tip if specified
    if let Some(tip_amount) = jito_tip_lamports {
        let jito_tip_address =
            SolanaPubkey::from_str("juLesoSmdTcRtzjCzYzRoHrnF8GhVu6KCV7uxq7nJGp")
                .map_err(|e| format!("Invalid Jito tip address: {}", e))?;

        // Create transfer instruction for Jito tip
        // System program ID: 11111111111111111111111111111111
        let system_program_id = SolanaPubkey::from_str("11111111111111111111111111111111")
            .map_err(|e| format!("Invalid system program ID: {}", e))?;

        let tip_ix = SolanaInstruction {
            program_id: system_program_id,
            accounts: vec![
                SolanaAccountMeta::new(payer, true),
                SolanaAccountMeta::new(jito_tip_address, false),
            ],
            data: {
                // Encode SystemInstruction::Transfer manually
                // Transfer instruction discriminator is 2, followed by lamports as u64
                let mut data = vec![2u8]; // Transfer instruction discriminator
                data.extend_from_slice(&tip_amount.to_le_bytes());
                data
            },
        };
        instructions.push(tip_ix);
    }

    // Fetch lookup table accounts if any are provided
    let lookup_table_accounts = if !route.address_lookup_tables.is_empty() {
        // Convert Titan pubkeys to Solana pubkeys
        let lookup_table_pubkeys: Result<Vec<SolanaPubkey>, String> = route
            .address_lookup_tables
            .iter()
            .map(titan_pubkey_to_solana)
            .collect();

        let lookup_table_pubkeys = lookup_table_pubkeys?;
        fetch_lookup_table_accounts(&lookup_table_pubkeys, rpc_url).await?
    } else {
        vec![]
    };

    // Build V0 message with lookup tables
    let message = v0::Message::try_compile(
        &payer,
        &instructions,
        &lookup_table_accounts,
        recent_blockhash,
    )
    .map_err(|e| format!("Failed to compile V0 message: {}", e))?;

    // Create versioned message
    let versioned_message = VersionedMessage::V0(message);

    // Create transaction with placeholder signature
    let transaction = VersionedTransaction {
        signatures: vec![SolanaSignature::default()],
        message: versioned_message,
    };

    // Serialize to bytes
    let serialized = bincode::serialize(&transaction)
        .map_err(|e| format!("Failed to serialize transaction: {}", e))?;

    Ok(serialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pubkey_conversion() {
        let titan_pubkey: Pubkey = [0u8; 32];
        let result = titan_pubkey_to_solana(&titan_pubkey);
        assert!(result.is_ok());
    }
}

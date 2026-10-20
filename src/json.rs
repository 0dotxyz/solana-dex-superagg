//! Parsers for the JSON the HTTP venues return.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;

/// An amount, which the venues send as a decimal string.
pub(crate) fn parse_u64(value: &Value) -> Result<u64> {
    value
        .as_str()
        .and_then(|amount| amount.parse().ok())
        .ok_or_else(|| anyhow!("invalid amount {}", value))
}

pub(crate) fn parse_bool(value: &Value) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| anyhow!("invalid bool {}", value))
}

pub(crate) fn parse_pubkey(value: &Value) -> Result<Pubkey> {
    value
        .as_str()
        .and_then(|key| Pubkey::from_str(key).ok())
        .ok_or_else(|| anyhow!("invalid pubkey {}", value))
}

/// An instruction as `{programId, accounts: [{pubkey, isSigner, isWritable}], data}` with base64
/// data, the shape Jupiter-like swap APIs use.
pub(crate) fn parse_instruction(value: &Value) -> Result<Instruction> {
    let accounts = value["accounts"]
        .as_array()
        .ok_or_else(|| anyhow!("instruction without accounts: {}", value))?
        .iter()
        .map(|meta| {
            Ok(AccountMeta {
                pubkey: parse_pubkey(&meta["pubkey"])?,
                is_signer: parse_bool(&meta["isSigner"])?,
                is_writable: parse_bool(&meta["isWritable"])?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Instruction {
        program_id: parse_pubkey(&value["programId"])?,
        accounts,
        data: value["data"]
            .as_str()
            .and_then(|data| STANDARD.decode(data).ok())
            .ok_or_else(|| anyhow!("instruction data is not base64: {}", value))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn amounts_are_decimal_strings() {
        assert_eq!(parse_u64(&json!("123")).unwrap(), 123);
        assert!(parse_u64(&json!(123)).is_err());
        assert!(parse_u64(&json!("-1")).is_err());
        assert!(parse_u64(&Value::Null).is_err());
    }

    #[test]
    fn an_instruction_is_parsed_with_its_accounts_and_base64_data() {
        let (program, account) = (Pubkey::new_unique(), Pubkey::new_unique());
        let value = json!({
            "programId": program.to_string(),
            "accounts": [
                { "pubkey": account.to_string(), "isSigner": true, "isWritable": false }
            ],
            "data": STANDARD.encode([1, 2, 3]),
        });
        let instruction = parse_instruction(&value).unwrap();
        assert_eq!(instruction.program_id, program);
        assert_eq!(
            instruction.accounts,
            [AccountMeta {
                pubkey: account,
                is_signer: true,
                is_writable: false
            }]
        );
        assert_eq!(instruction.data, [1, 2, 3]);

        for broken in [
            json!({ "programId": "nope", "accounts": [], "data": "" }),
            json!({ "programId": program.to_string(), "data": "" }),
            json!({ "programId": program.to_string(), "accounts": [], "data": "%%%" }),
            json!({
                "programId": program.to_string(),
                "accounts": [{ "pubkey": account.to_string(), "isSigner": "yes", "isWritable": false }],
                "data": ""
            }),
        ] {
            assert!(parse_instruction(&broken).is_err(), "{}", broken);
        }
    }
}

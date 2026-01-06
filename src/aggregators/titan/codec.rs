//! MessagePack encoding/decoding utilities for Titan API

use solana_sdk::pubkey::Pubkey as SolanaPubkey;
use std::str::FromStr;

/// Convert base58 string to 32-byte array for MessagePack encoding
pub fn base58_to_bytes(base58: &str) -> Result<[u8; 32], String> {
    let pubkey =
        SolanaPubkey::from_str(base58).map_err(|e| format!("Invalid base58 pubkey: {}", e))?;
    Ok(pubkey.to_bytes())
}

/// Convert 32-byte array to base58 string
#[allow(dead_code)]
pub fn bytes_to_base58(bytes: &[u8; 32]) -> String {
    let pubkey = SolanaPubkey::try_from(bytes.as_slice()).unwrap();
    pubkey.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pubkey_conversion() {
        let pubkey_str = "So11111111111111111111111111111111111111112";
        let bytes = base58_to_bytes(pubkey_str).unwrap();
        let converted_back = bytes_to_base58(&bytes);
        assert_eq!(pubkey_str, converted_back);
    }
}

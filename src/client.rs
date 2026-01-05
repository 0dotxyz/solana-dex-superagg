use crate::config::ClientConfig;
use anyhow::Result;
use solana_client::{rpc_client::RpcClient, rpc_config::CommitmentConfig};
use solana_sdk::signature::{Keypair, Signer};

/// Main client for routing orders across multiple DEX aggregators (Jupiter, Titan, etc.)
pub struct DexSuperAggClient {
    /// Wallet keypair for signing transactions
    signer: Keypair,
    /// Solana RPC client
    rpc_client: RpcClient,
    /// Client configuration
    config: ClientConfig,
}

impl DexSuperAggClient {
    /// Create a new DexSuperAggClient from configuration
    /// Requires a wallet keypair to be configured
    pub fn new(config: ClientConfig) -> Result<Self> {
        let signer = config
            .get_keypair()?
            .ok_or_else(|| anyhow::anyhow!("Wallet keypair is required for DexSuperAggClient"))?;

        let rpc_client =
            RpcClient::new_with_commitment(&config.shared.rpc_url, CommitmentConfig::confirmed());

        Ok(Self {
            signer,
            rpc_client,
            config,
        })
    }

    /// Create a new DexSuperAggClient from environment variables
    pub fn from_env() -> Result<Self> {
        let config = ClientConfig::from_env()?;
        Self::new(config)
    }

    /// Get the signer's public key
    pub fn pubkey(&self) -> solana_sdk::pubkey::Pubkey {
        self.signer.pubkey()
    }

    /// Get a reference to the configuration
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Get a reference to the RPC client
    pub fn rpc_client(&self) -> &RpcClient {
        &self.rpc_client
    }

    /// Get a reference to the signer
    pub fn signer(&self) -> &Keypair {
        &self.signer
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_client_creation() {
        // This test would require a valid keypair, so we'll skip it for now
        // In a real scenario, you'd use a test keypair
    }
}

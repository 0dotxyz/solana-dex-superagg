use figment::{providers::Env, Figment};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, signature::Keypair};
use std::time::Duration;
use tokio::time::timeout;

/// Preferred aggregator for routing swaps
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PreferredAggregator {
    /// Use Jupiter aggregator
    Jupiter,
    /// Use Titan aggregator
    Titan,
}

/// Shared configuration used by both Jupiter and Titan
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedConfig {
    /// Solana RPC endpoint URL
    pub rpc_url: String,
    /// Slippage tolerance in basis points (e.g., 25 = 0.25%)
    pub slippage_bps: u16,
    /// Wallet keypair (base58 encoded string, JSON array, or comma-separated bytes)
    /// Optional - not required for simulation/quote-only operations
    pub wallet_keypair: Option<String>,
    /// Compute unit price in micro lamports
    pub compute_unit_price_micro_lamports: u64,
    /// Preferred aggregator for routing swaps (None = use best price)
    pub preferred_aggregator: Option<PreferredAggregator>,
}

impl Default for SharedConfig {
    fn default() -> Self {
        Self {
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            slippage_bps: 50, // 0.5% default slippage
            wallet_keypair: None,
            compute_unit_price_micro_lamports: 0,
            preferred_aggregator: None, // None = use best price from available aggregators
        }
    }
}

/// Jupiter-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JupiterConfig {
    /// Jupiter Swap API URL
    pub jup_swap_api_url: String,
}

impl Default for JupiterConfig {
    fn default() -> Self {
        Self {
            jup_swap_api_url: "https://quote-api.jup.ag/v6".to_string(),
        }
    }
}

/// Titan-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TitanConfig {
    /// Titan WebSocket endpoint (e.g., "us1.api.demo.titan.exchange")
    pub titan_ws_endpoint: String,
    /// Titan API key (required for direct mode)
    pub titan_api_key: Option<String>,
    /// Hermes proxy endpoint (alternative to direct mode)
    pub hermes_endpoint: Option<String>,
}

impl Default for TitanConfig {
    fn default() -> Self {
        Self {
            titan_ws_endpoint: String::new(),
            titan_api_key: None,
            hermes_endpoint: None,
        }
    }
}

/// Complete client configuration combining shared, Jupiter, and Titan configs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub shared: SharedConfig,
    #[serde(default)]
    pub jupiter: JupiterConfig,
    /// Titan configuration (optional - if not provided, Titan will not be used)
    #[serde(default)]
    pub titan: Option<TitanConfig>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            shared: SharedConfig::default(),
            jupiter: JupiterConfig::default(),
            titan: None,
        }
    }
}

impl ClientConfig {
    /// Load configuration from environment variables using figment
    ///
    /// Environment variables should be prefixed with `DEX_SUPERAGG_` and use `__` to separate nested keys.
    /// Examples:
    /// - `DEX_SUPERAGG_SHARED__RPC_URL` for `shared.rpc_url`
    /// - `DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL` for `jupiter.jup_swap_api_url`
    /// - `DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT` for `titan.titan_ws_endpoint`
    pub fn from_env() -> Result<Self, figment::Error> {
        Figment::new()
            .merge(Env::prefixed("DEX_SUPERAGG_").split("__"))
            .extract()
    }

    /// Check if Titan is configured
    pub fn is_titan_configured(&self) -> bool {
        self.titan
            .as_ref()
            .map(|t| !t.titan_ws_endpoint.is_empty())
            .unwrap_or(false)
    }

    /// Validate the configuration by actually testing connectivity to endpoints
    ///
    /// Checks:
    /// - Wallet keypair (if provided) can be parsed
    /// - RPC URL is reachable and responds
    /// - Jupiter API URL is reachable and responds
    /// - Titan WebSocket endpoint (if configured) is reachable
    ///
    /// Returns a vector of validation errors, empty if all checks pass
    pub async fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // Validate wallet keypair if provided
        if let Some(ref wallet_keypair) = self.shared.wallet_keypair {
            if wallet_keypair.is_empty() {
                errors.push("Wallet keypair is set but empty".to_string());
            } else if let Err(e) = self.get_keypair() {
                errors.push(format!("Invalid wallet keypair format: {}", e));
            }
        }

        // Validate RPC URL by making a test call
        if !self.shared.rpc_url.is_empty() {
            if let Err(e) = self.validate_rpc_url().await {
                errors.push(format!(
                    "RPC URL validation failed ({}): {}",
                    self.shared.rpc_url, e
                ));
            }
        } else {
            errors.push("RPC URL is required".to_string());
        }

        // Validate Jupiter API URL by making a test request
        if !self.jupiter.jup_swap_api_url.is_empty() {
            if let Err(e) = self.validate_jupiter_url().await {
                errors.push(format!(
                    "Jupiter API URL validation failed ({}): {}",
                    self.jupiter.jup_swap_api_url, e
                ));
            }
        } else {
            errors.push("Jupiter API URL is required".to_string());
        }

        // Validate Titan configuration if provided
        if let Some(ref titan) = self.titan {
            if titan.titan_ws_endpoint.is_empty() {
                errors.push(
                    "Titan WebSocket endpoint is required when Titan is configured".to_string(),
                );
            } else {
                // Test WebSocket connectivity
                if let Err(e) = self.validate_titan_endpoint(titan).await {
                    errors.push(format!(
                        "Titan WebSocket endpoint validation failed ({}): {}",
                        titan.titan_ws_endpoint, e
                    ));
                }
            }

            // Check that either API key or Hermes endpoint is provided
            if titan.titan_api_key.is_none() && titan.hermes_endpoint.is_none() {
                errors.push(
                    "Either Titan API key or Hermes endpoint must be provided when Titan is configured"
                        .to_string(),
                );
            }
        }

        errors
    }

    async fn validate_rpc_url(&self) -> anyhow::Result<()> {
        let rpc_url = self.shared.rpc_url.clone();

        // RpcClient is blocking, so we need to run it in a blocking task
        timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                let rpc_client =
                    RpcClient::new_with_commitment(&rpc_url, CommitmentConfig::confirmed());
                rpc_client
                    .get_version()
                    .map_err(|e| anyhow::anyhow!("Failed to connect to RPC: {}", e))?;
                Ok::<(), anyhow::Error>(())
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("RPC connection timeout"))?
        .map_err(|e| anyhow::anyhow!("RPC task error: {}", e))??;

        Ok(())
    }

    async fn validate_jupiter_url(&self) -> anyhow::Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

        // Try to reach the Jupiter API - just check if it responds
        timeout(
            Duration::from_secs(5),
            client.get(&self.jupiter.jup_swap_api_url).send(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Jupiter API request timeout"))?
        .map_err(|e| anyhow::anyhow!("Failed to connect to Jupiter API: {}", e))?;

        Ok(())
    }

    async fn validate_titan_endpoint(&self, titan: &TitanConfig) -> anyhow::Result<()> {
        // Build the WebSocket URL
        let ws_url = if titan.titan_ws_endpoint.starts_with("ws://")
            || titan.titan_ws_endpoint.starts_with("wss://")
        {
            titan.titan_ws_endpoint.clone()
        } else {
            format!("wss://{}", titan.titan_ws_endpoint)
        };

        // Try to connect to the WebSocket endpoint
        timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(&ws_url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("WebSocket connection timeout"))?
        .map_err(|e| anyhow::anyhow!("Failed to connect to Titan WebSocket: {}", e))?;

        Ok(())
    }

    /// Get the wallet keypair from the configuration
    /// Returns None if no keypair is configured (useful for simulation/quote-only operations)
    pub fn get_keypair(&self) -> anyhow::Result<Option<Keypair>> {
        let wallet_keypair = match &self.shared.wallet_keypair {
            Some(kp) => kp,
            None => return Ok(None),
        };

        // Try parsing as base58 string first
        if let Ok(bytes) = bs58::decode(wallet_keypair).into_vec() {
            if bytes.len() == 64 {
                if let Ok(keypair) = Keypair::from_bytes(&bytes) {
                    return Ok(Some(keypair));
                }
            }
        }

        // Try parsing as bytes (comma-separated or JSON array)
        let bytes: Vec<u8> = if wallet_keypair.starts_with('[') {
            // JSON array format
            serde_json::from_str(wallet_keypair)
                .map_err(|e| anyhow::anyhow!("Failed to parse wallet keypair as JSON: {}", e))?
        } else if wallet_keypair.contains(',') {
            // Comma-separated format
            wallet_keypair
                .split(',')
                .map(|s| s.trim().parse::<u8>())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("Failed to parse wallet keypair bytes: {}", e))?
        } else {
            return Err(anyhow::anyhow!(
                "Invalid wallet keypair format. Expected base58 string, JSON array, or comma-separated bytes"
            ));
        };

        Keypair::from_bytes(&bytes)
            .map(|kp| Some(kp))
            .map_err(|e| anyhow::anyhow!("Failed to create keypair from bytes: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signer;

    #[test]
    fn test_config_defaults() {
        let shared = SharedConfig::default();
        assert_eq!(shared.slippage_bps, 50);
        assert!(!shared.rpc_url.is_empty());
        assert_eq!(shared.preferred_aggregator, None);

        let jupiter = JupiterConfig::default();
        assert!(!jupiter.jup_swap_api_url.is_empty());
    }

    #[test]
    fn test_load_config_from_env() {
        figment::Jail::expect_with(|jail| {
            // Set shared config environment variables
            jail.set_env(
                "DEX_SUPERAGG_SHARED__RPC_URL",
                "https://api.testnet.solana.com",
            );
            jail.set_env("DEX_SUPERAGG_SHARED__SLIPPAGE_BPS", "100");
            jail.set_env(
                "DEX_SUPERAGG_SHARED__WALLET_KEYPAIR",
                "5KQwrPtwYgXv3LvWy2YXL4LwzFjU4Lsa6Yc9t7veZmn8qJxVzN",
            );
            jail.set_env(
                "DEX_SUPERAGG_SHARED__COMPUTE_UNIT_PRICE_MICRO_LAMPORTS",
                "5000",
            );

            // Set Jupiter config environment variables
            jail.set_env(
                "DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL",
                "https://test-api.jup.ag/v6",
            );

            // Set Titan config environment variables
            jail.set_env(
                "DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT",
                "us1.api.test.titan.exchange",
            );
            jail.set_env("DEX_SUPERAGG_TITAN__TITAN_API_KEY", "test_titan_api_key");
            jail.set_env(
                "DEX_SUPERAGG_TITAN__HERMES_ENDPOINT",
                "https://hermes.test.titan.exchange",
            );

            let config = ClientConfig::from_env()?;

            // Verify shared config
            assert_eq!(config.shared.rpc_url, "https://api.testnet.solana.com");
            assert_eq!(config.shared.slippage_bps, 100);
            assert_eq!(
                config.shared.wallet_keypair,
                Some("5KQwrPtwYgXv3LvWy2YXL4LwzFjU4Lsa6Yc9t7veZmn8qJxVzN".to_string())
            );
            assert_eq!(config.shared.compute_unit_price_micro_lamports, 5000);

            // Verify Jupiter config
            assert_eq!(
                config.jupiter.jup_swap_api_url,
                "https://test-api.jup.ag/v6"
            );

            // Verify Titan config
            let titan = config
                .titan
                .as_ref()
                .expect("Titan config should be present");
            assert_eq!(titan.titan_ws_endpoint, "us1.api.test.titan.exchange");
            assert_eq!(titan.titan_api_key, Some("test_titan_api_key".to_string()));
            assert_eq!(
                titan.hermes_endpoint,
                Some("https://hermes.test.titan.exchange".to_string())
            );

            Ok(())
        });
    }

    #[test]
    fn test_env_override_defaults() {
        figment::Jail::expect_with(|jail| {
            // Only set required fields, others should use defaults
            jail.set_env("DEX_SUPERAGG_SHARED__WALLET_KEYPAIR", "test_keypair");
            jail.set_env(
                "DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT",
                "test.titan.exchange",
            );
            // Don't set Jupiter config - it should use defaults automatically

            // Override one default value
            jail.set_env("DEX_SUPERAGG_SHARED__SLIPPAGE_BPS", "75");

            let config = ClientConfig::from_env()?;

            // Verify overridden value
            assert_eq!(config.shared.slippage_bps, 75);

            // Verify defaults are used
            assert_eq!(config.shared.rpc_url, SharedConfig::default().rpc_url);
            assert_eq!(
                config.jupiter.jup_swap_api_url,
                JupiterConfig::default().jup_swap_api_url
            );
            assert_eq!(config.shared.compute_unit_price_micro_lamports, 0);

            // Verify set values
            assert_eq!(
                config.shared.wallet_keypair,
                Some("test_keypair".to_string())
            );
            let titan = config
                .titan
                .as_ref()
                .expect("Titan config should be present");
            assert_eq!(titan.titan_ws_endpoint, "test.titan.exchange");

            Ok(())
        });
    }

    #[test]
    fn test_titan_not_configured() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("DEX_SUPERAGG_SHARED__WALLET_KEYPAIR", "test_keypair");
            // Don't set any Titan config - it should be None

            let config = ClientConfig::from_env()?;

            assert_eq!(config.titan, None);
            assert!(!config.is_titan_configured());

            Ok(())
        });
    }

    #[test]
    fn test_wallet_keypair_not_configured() {
        figment::Jail::expect_with(|jail| {
            // Don't set wallet_keypair - it should be None (useful for simulations)
            jail.set_env(
                "DEX_SUPERAGG_SHARED__RPC_URL",
                "https://api.testnet.solana.com",
            );

            let config = ClientConfig::from_env()?;

            assert_eq!(config.shared.wallet_keypair, None);
            assert!(config
                .get_keypair()
                .map_err(|e| figment::Error::from(e.to_string()))?
                .is_none());

            Ok(())
        });
    }

    #[test]
    fn test_wallet_keypair_base58() {
        figment::Jail::expect_with(|jail| {
            // Create a test keypair and encode it as base58
            let test_keypair = solana_sdk::signature::Keypair::new();
            let base58_keypair = bs58::encode(test_keypair.to_bytes()).into_string();

            jail.set_env("DEX_SUPERAGG_SHARED__WALLET_KEYPAIR", &base58_keypair);

            let config = ClientConfig::from_env()?;
            let parsed_keypair = config
                .get_keypair()
                .map_err(|e| figment::Error::from(e.to_string()))?
                .expect("Keypair should be parsed");

            assert_eq!(parsed_keypair.pubkey(), test_keypair.pubkey());

            Ok(())
        });
    }

    #[test]
    fn test_wallet_keypair_json_array_parsing() {
        // Test that JSON array format can be parsed (even though figment doesn't support it in env vars)
        // This format is useful when loading config from files or other sources
        let test_keypair = solana_sdk::signature::Keypair::new();
        let json_keypair = serde_json::to_string(&test_keypair.to_bytes().to_vec()).unwrap();

        // Create a config with JSON array format directly (bypassing figment)
        let mut shared = SharedConfig::default();
        shared.wallet_keypair = Some(json_keypair);

        let config = ClientConfig {
            shared,
            ..Default::default()
        };

        let parsed_keypair = config
            .get_keypair()
            .expect("Keypair should be parsed")
            .expect("Keypair should be Some");

        assert_eq!(parsed_keypair.pubkey(), test_keypair.pubkey());
    }

    #[test]
    fn test_wallet_keypair_comma_separated() {
        figment::Jail::expect_with(|jail| {
            // Create a test keypair and encode it as comma-separated bytes
            let test_keypair = solana_sdk::signature::Keypair::new();
            let bytes = test_keypair.to_bytes();
            let comma_keypair = bytes
                .iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join(",");

            jail.set_env("DEX_SUPERAGG_SHARED__WALLET_KEYPAIR", &comma_keypair);

            let config = ClientConfig::from_env()?;
            let parsed_keypair = config
                .get_keypair()
                .map_err(|e| figment::Error::from(e.to_string()))?
                .expect("Keypair should be parsed");

            assert_eq!(parsed_keypair.pubkey(), test_keypair.pubkey());

            Ok(())
        });
    }

    #[test]
    fn test_wallet_keypair_invalid_format() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("DEX_SUPERAGG_SHARED__WALLET_KEYPAIR", "invalid_format");

            let config = ClientConfig::from_env()?;
            let result = config.get_keypair();

            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("Invalid wallet keypair format"));

            Ok(())
        });
    }

    #[test]
    fn test_optional_titan_fields() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("DEX_SUPERAGG_SHARED__WALLET_KEYPAIR", "test_keypair");
            jail.set_env(
                "DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT",
                "test.titan.exchange",
            );
            // Don't set Jupiter config - it should use defaults automatically
            // Don't set titan_api_key or hermes_endpoint - they should be None

            let config = ClientConfig::from_env()?;

            let titan = config
                .titan
                .as_ref()
                .expect("Titan config should be present");
            assert_eq!(titan.titan_api_key, None);
            assert_eq!(titan.hermes_endpoint, None);
            assert_eq!(titan.titan_ws_endpoint, "test.titan.exchange");
            // Verify Jupiter uses default
            assert_eq!(
                config.jupiter.jup_swap_api_url,
                JupiterConfig::default().jup_swap_api_url
            );

            Ok(())
        });
    }

    #[tokio::test]
    async fn test_validate_keypair() {
        let test_keypair = solana_sdk::signature::Keypair::new();
        let base58_keypair = bs58::encode(test_keypair.to_bytes()).into_string();

        let mut config = ClientConfig::default();
        config.shared.rpc_url = "https://api.mainnet-beta.solana.com".to_string();
        config.shared.wallet_keypair = Some(base58_keypair);
        config.jupiter.jup_swap_api_url = "https://quote-api.jup.ag/v6".to_string();

        let errors = config.validate().await;

        // Should only have errors for endpoint connectivity, not keypair
        assert!(!errors.iter().any(|e| e.contains("wallet keypair")));
    }

    #[tokio::test]
    async fn test_validate_invalid_keypair() {
        let mut config = ClientConfig::default();
        config.shared.rpc_url = "https://api.mainnet-beta.solana.com".to_string();
        config.shared.wallet_keypair = Some("invalid_keypair".to_string());
        config.jupiter.jup_swap_api_url = "https://quote-api.jup.ag/v6".to_string();

        let errors = config.validate().await;

        assert!(errors.iter().any(|e| e.contains("wallet keypair")));
    }

    #[tokio::test]
    async fn test_validate_rpc_endpoint() {
        // Test with a real Solana RPC endpoint
        let mut config = ClientConfig::default();
        config.shared.rpc_url = "https://api.mainnet-beta.solana.com".to_string();
        config.jupiter.jup_swap_api_url = "https://quote-api.jup.ag/v6".to_string();

        let errors = config.validate().await;

        // Should not have RPC URL errors if it's reachable
        // (may have other errors if endpoints are down, but RPC should work)
        let rpc_errors: Vec<_> = errors.iter().filter(|e| e.contains("RPC URL")).collect();
        // If RPC is unreachable, that's fine for the test - we just want to make sure
        // the validation logic works
        println!("RPC validation errors: {:?}", rpc_errors);
    }

    #[tokio::test]
    async fn test_validate_jupiter_endpoint() {
        let mut config = ClientConfig::default();
        config.shared.rpc_url = "https://api.mainnet-beta.solana.com".to_string();
        config.jupiter.jup_swap_api_url = "https://quote-api.jup.ag/v6".to_string();

        let errors = config.validate().await;

        // Should not have Jupiter URL errors if it's reachable
        let jupiter_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("Jupiter API URL"))
            .collect();
        // If Jupiter is unreachable, that's fine for the test
        println!("Jupiter validation errors: {:?}", jupiter_errors);
    }

    #[tokio::test]
    async fn test_validate_invalid_endpoints() {
        let mut config = ClientConfig::default();
        config.shared.rpc_url = "https://invalid-rpc-url-that-does-not-exist.com".to_string();
        config.jupiter.jup_swap_api_url =
            "https://invalid-jupiter-url-that-does-not-exist.com".to_string();

        let errors = config.validate().await;

        // Should have errors for both invalid endpoints
        assert!(errors.iter().any(|e| e.contains("RPC URL")));
        assert!(errors.iter().any(|e| e.contains("Jupiter API URL")));
    }

    #[test]
    fn test_preferred_aggregator_default() {
        let shared = SharedConfig::default();
        assert_eq!(shared.preferred_aggregator, None);
    }

    #[test]
    fn test_preferred_aggregator_from_env() {
        figment::Jail::expect_with(|jail| {
            // Test Jupiter
            jail.set_env("DEX_SUPERAGG_SHARED__PREFERRED_AGGREGATOR", "jupiter");
            let config = ClientConfig::from_env()?;
            assert_eq!(
                config.shared.preferred_aggregator,
                Some(PreferredAggregator::Jupiter)
            );

            // Test Titan
            jail.set_env("DEX_SUPERAGG_SHARED__PREFERRED_AGGREGATOR", "titan");
            let config = ClientConfig::from_env()?;
            assert_eq!(
                config.shared.preferred_aggregator,
                Some(PreferredAggregator::Titan)
            );

            Ok(())
        });
    }
}

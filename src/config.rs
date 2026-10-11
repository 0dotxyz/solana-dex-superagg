use std::fmt;
use std::time::Duration;

/// A DEX aggregator venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregator {
    Jupiter,
    Titan,
}

/// Which venues to quote.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum RoutingStrategy {
    /// Quote every configured venue and build the one with the most output.
    #[default]
    BestPrice,
    /// Quote and build only this venue.
    PreferredAggregator { aggregator: Aggregator },
}

#[derive(Clone)]
pub struct JupiterConfig {
    /// e.g. `https://api.jup.ag/swap/v1`
    pub jup_swap_api_url: String,
    pub api_key: String,
}

#[derive(Clone)]
pub struct TitanConfig {
    /// A host (`us1.api.titan.exchange`) or a proxy URL (`http://localhost:8080/ws`)
    pub titan_ws_endpoint: String,
    pub titan_api_key: String,
}

#[derive(Clone)]
pub struct ClientConfig {
    pub rpc_url: String,
    pub slippage_bps: u16,
    /// Priority fee per compute unit; 0 leaves each venue's default.
    pub compute_unit_price_micro_lamports: u64,
    pub routing_strategy: RoutingStrategy,
    /// Per-venue quote deadline; a venue that misses it is left out of the comparison.
    pub quote_timeout: Duration,
    /// Per-venue deadline for building a transaction from its quote.
    pub build_timeout: Duration,
    /// How long `send_and_confirm` waits for the transaction.
    pub confirm_timeout: Duration,
    pub jupiter: Option<JupiterConfig>,
    pub titan: Option<TitanConfig>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            slippage_bps: 50,
            compute_unit_price_micro_lamports: 0,
            routing_strategy: RoutingStrategy::BestPrice,
            quote_timeout: Duration::from_secs(5),
            build_timeout: Duration::from_secs(10),
            confirm_timeout: Duration::from_secs(90),
            jupiter: None,
            titan: None,
        }
    }
}

/// Scheme and host only: RPC URLs commonly embed an API token.
fn redact_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..host_end].rsplit('@').next().unwrap_or_default();
    let suffix = if host_end < rest.len() { "/…" } else { "" };
    if scheme.is_empty() {
        format!("{host}{suffix}")
    } else {
        format!("{scheme}://{host}{suffix}")
    }
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("rpc_url", &redact_url(&self.rpc_url))
            .field("slippage_bps", &self.slippage_bps)
            .field(
                "compute_unit_price_micro_lamports",
                &self.compute_unit_price_micro_lamports,
            )
            .field("routing_strategy", &self.routing_strategy)
            .field("quote_timeout", &self.quote_timeout)
            .field("build_timeout", &self.build_timeout)
            .field("confirm_timeout", &self.confirm_timeout)
            .field("jupiter", &self.jupiter)
            .field("titan", &self.titan)
            .finish()
    }
}

impl fmt::Debug for JupiterConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JupiterConfig")
            .field("jup_swap_api_url", &self.jup_swap_api_url)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for TitanConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TitanConfig")
            .field("titan_ws_endpoint", &self.titan_ws_endpoint)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_hides_credentials() {
        let config = ClientConfig {
            rpc_url: "https://rpc.example.com/secret-rpc-token".to_string(),
            jupiter: Some(JupiterConfig {
                jup_swap_api_url: "https://api.jup.ag/swap/v1".to_string(),
                api_key: "secret-jup".to_string(),
            }),
            titan: Some(TitanConfig {
                titan_ws_endpoint: "us1.api.titan.exchange".to_string(),
                titan_api_key: "secret-titan".to_string(),
            }),
            ..Default::default()
        };
        let printed = format!("{:?}", config);
        for secret in ["secret-rpc-token", "secret-jup", "secret-titan"] {
            assert!(
                !printed.contains(secret),
                "{} leaked in {}",
                secret,
                printed
            );
        }
        assert!(printed.contains("https://rpc.example.com/…"));
    }
}

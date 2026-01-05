//! Jupiter aggregator implementation
//!
//! This module implements the `DexAggregator` trait for Jupiter Swap API.

use crate::aggregators::{DexAggregator, QuoteMetadata, SimulateResult, SwapResult};
use crate::config::ClientConfig;
use anyhow::{anyhow, Result};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::str::FromStr;

/// Jupiter aggregator implementation
pub struct JupiterAggregator {
    /// Jupiter Swap API client
    api_url: String,
    /// Solana RPC client for transaction submission
    rpc_client: RpcClient,
    /// Wallet keypair for signing transactions
    signer: Keypair,
    /// Compute unit price in micro lamports
    compute_unit_price_micro_lamports: u64,
}

impl JupiterAggregator {
    /// Create a new Jupiter aggregator from configuration
    pub fn new(config: &ClientConfig, signer: Keypair) -> Result<Self> {
        let rpc_client =
            RpcClient::new_with_commitment(&config.shared.rpc_url, CommitmentConfig::confirmed());

        Ok(Self {
            api_url: config.jupiter.jup_swap_api_url.clone(),
            rpc_client,
            signer,
            compute_unit_price_micro_lamports: config.shared.compute_unit_price_micro_lamports,
        })
    }

    /// Create a new Jupiter aggregator with explicit configuration
    pub fn with_config(
        jupiter_api_url: &str,
        rpc_url: &str,
        signer: Keypair,
        compute_unit_price_micro_lamports: u64,
    ) -> Result<Self> {
        let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

        Ok(Self {
            api_url: jupiter_api_url.to_string(),
            rpc_client,
            signer,
            compute_unit_price_micro_lamports,
        })
    }

    /// Make a quote request to Jupiter API
    async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<JupiterQuoteResponse> {
        let input_pubkey = Pubkey::from_str(input_mint)
            .map_err(|e| anyhow!("Invalid input mint address: {}", e))?;
        let output_pubkey = Pubkey::from_str(output_mint)
            .map_err(|e| anyhow!("Invalid output mint address: {}", e))?;

        if input_pubkey == output_pubkey {
            return Err(anyhow!("Input and output mints cannot be the same"));
        }

        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.api_url, input_mint, output_mint, amount, slippage_bps
        );

        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to request Jupiter quote: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jupiter API error ({}): {}", status, text));
        }

        let quote: JupiterQuoteResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse Jupiter quote response: {}", e))?;

        Ok(quote)
    }

    /// Execute a swap using Jupiter Swap API
    async fn execute_swap(&self, quote_response: &JupiterQuoteResponse) -> Result<String> {
        let swap_url = format!("{}/swap", self.api_url);

        let swap_request = JupiterSwapRequest {
            quote_response: quote_response.clone(),
            user_public_key: self.signer.pubkey().to_string(),
            wrap_and_unwrap_sol: false,
            compute_unit_price_micro_lamports: if self.compute_unit_price_micro_lamports > 0 {
                Some(self.compute_unit_price_micro_lamports)
            } else {
                None
            },
        };

        let client = reqwest::Client::new();
        let response = client
            .post(&swap_url)
            .json(&swap_request)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to request Jupiter swap: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Jupiter swap API error ({}): {}", status, text));
        }

        let swap_response: JupiterSwapResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse Jupiter swap response: {}", e))?;

        // Decode base64 transaction and deserialize
        use base64::{engine::general_purpose, Engine as _};
        let tx_bytes = general_purpose::STANDARD
            .decode(&swap_response.swap_transaction)
            .map_err(|e| anyhow!("Failed to decode base64 transaction: {}", e))?;

        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| anyhow!("Failed to deserialize Jupiter transaction: {}", e))?;

        // Sign transaction
        tx = VersionedTransaction::try_new(tx.message, &[&self.signer])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))?;

        // Send transaction using spawn_blocking to avoid blocking the async runtime
        let rpc_client = self.rpc_client.clone();
        let sig = tokio::task::spawn_blocking(move || {
            rpc_client.send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
        })
        .await
        .map_err(|e| anyhow!("Failed to spawn blocking task: {}", e))?
        .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

        Ok(sig.to_string())
    }
}

#[async_trait::async_trait]
impl DexAggregator for JupiterAggregator {
    async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<SwapResult> {
        // Get quote
        let quote = self.get_quote(input, output, amount, slippage_bps).await?;

        // Execute swap
        let signature = self.execute_swap(&quote).await?;

        Ok(SwapResult {
            signature,
            out_amount: quote.out_amount,
        })
    }

    async fn simulate(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<SimulateResult> {
        let quote = self.get_quote(input, output, amount, slippage_bps).await?;

        // Calculate price impact
        // Price impact = (in_amount * in_price - out_amount * out_price) / (in_amount * in_price)
        // For simplicity, we'll use the price impact from the quote if available,
        // otherwise calculate a rough estimate
        let price_impact = quote.price_impact_pct.unwrap_or(0.0);

        // Extract route information
        let route = quote
            .route
            .as_ref()
            .map(|r| {
                r.iter()
                    .map(|step| {
                        format!(
                            "{} -> {}",
                            step.swap_info
                                .label
                                .as_ref()
                                .unwrap_or(&"Unknown".to_string()),
                            step.swap_info.output_mint
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" -> ")
            })
            .or_else(|| {
                quote.market_infos.as_ref().map(|infos| {
                    infos
                        .iter()
                        .map(|info| {
                            format!(
                                "{} -> {}",
                                info.label.as_ref().unwrap_or(&"Unknown".to_string()),
                                info.output_mint
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" -> ")
                })
            });

        Ok(SimulateResult {
            out_amount: quote.out_amount,
            price_impact,
            metadata: QuoteMetadata {
                route,
                fees: quote.platform_fee.map(|f| f.amount),
                extra: Some(serde_json::json!({
                    "in_amount": quote.in_amount,
                    "context_slot": quote.context_slot,
                })),
            },
        })
    }
}

/// Jupiter quote response structure
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterQuoteResponse {
    input_mint: String,
    in_amount: String,
    output_mint: String,
    out_amount: u64,
    other_amount_threshold: String,
    swap_mode: String,
    slippage_bps: u16,
    platform_fee: Option<PlatformFee>,
    price_impact_pct: Option<f64>,
    route: Option<Vec<RouteStep>>,
    context_slot: Option<u64>,
    time_taken: Option<f64>,
    #[serde(default)]
    market_infos: Option<Vec<MarketInfo>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlatformFee {
    amount: u64,
    fee_bps: u16,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteStep {
    swap_info: SwapInfo,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapInfo {
    amm_key: String,
    label: Option<String>,
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    fee_amount: String,
    fee_mint: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketInfo {
    id: String,
    label: Option<String>,
    input_mint: String,
    output_mint: String,
    not_enough_liquidity: bool,
    in_amount: String,
    out_amount: String,
}

/// Jupiter swap request structure
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JupiterSwapRequest {
    quote_response: JupiterQuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_price_micro_lamports: Option<u64>,
}

/// Jupiter swap response structure
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterSwapResponse {
    swap_transaction: String, // Base64 encoded transaction
}

use crate::aggregators::{DexAggregator, QuoteMetadata, QuoteResult, SwapResult, SwapSummary};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentLevel,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

/// Quote response from DFlow API
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct QuoteResponse {
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    #[serde(default, deserialize_with = "deserialize_price_impact")]
    price_impact_pct: f64,
    #[serde(default)]
    route_plan: Option<serde_json::Value>,
    // Additional fields from DFlow API
    #[serde(default)]
    other_amount_threshold: Option<String>,
    #[serde(default)]
    min_out_amount: Option<String>,
    #[serde(default)]
    slippage_bps: Option<u16>,
    #[serde(default)]
    platform_fee: Option<serde_json::Value>,
    #[serde(default)]
    out_transfer_fee: Option<serde_json::Value>,
    #[serde(default)]
    context_slot: Option<u64>,
    #[serde(default)]
    simulated_compute_units: Option<u64>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    for_jito_bundle: Option<bool>,
}

/// Custom deserializer for price_impact_pct that accepts both string and f64
fn deserialize_price_impact<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct PriceImpactVisitor;

    impl<'de> Visitor<'de> for PriceImpactVisitor {
        type Value = f64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or f64 for price impact percentage")
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            value.parse::<f64>().map_err(de::Error::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            value.parse::<f64>().map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_any(PriceImpactVisitor)
}

/// Swap request body for DFlow API
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapRequest {
    user_public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    dynamic_compute_unit_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prioritization_fee_lamports: Option<u64>,
    quote_response: QuoteResponseForSwap,
}

/// Quote response format for swap requests (priceImpactPct must be string)
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct QuoteResponseForSwap {
    input_mint: String,
    output_mint: String,
    in_amount: String,
    out_amount: String,
    #[serde(serialize_with = "serialize_price_impact")]
    price_impact_pct: f64,
    #[serde(default)]
    route_plan: Option<serde_json::Value>,
    // Required fields for swap request
    #[serde(skip_serializing_if = "Option::is_none")]
    other_amount_threshold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_out_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slippage_bps: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform_fee: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    out_transfer_fee: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_slot: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    simulated_compute_units: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    for_jito_bundle: Option<bool>,
}

/// Custom serializer for price_impact_pct that converts f64 to string
fn serialize_price_impact<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

/// Swap response from DFlow API
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    swap_transaction: String, // base64-encoded VersionedTransaction
}

/// DFlow DEX aggregator implementation
pub struct DflowAggregator {
    /// HTTP client for DFlow API
    http_client: reqwest::Client,
    /// Solana RPC client
    rpc_client: RpcClient,
    /// Wallet keypair for signing transactions
    signer: Arc<Keypair>,
    /// Compute unit price in micro lamports
    compute_unit_price_micro_lamports: u64,
    /// DFlow API URL
    api_url: String,
    /// Optional API key
    api_key: Option<String>,
}

impl DflowAggregator {
    /// Create a new DflowAggregator from ClientConfig
    pub fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        Self::new_with_compute_price(
            config,
            signer,
            config.shared.compute_unit_price_micro_lamports,
        )
    }

    /// Create a new DflowAggregator with custom compute unit price
    pub fn new_with_compute_price(
        config: &ClientConfig,
        signer: Arc<Keypair>,
        compute_unit_price_micro_lamports: u64,
    ) -> Result<Self> {
        let dflow_config = config
            .dflow
            .as_ref()
            .ok_or_else(|| anyhow!("DFlow configuration not found"))?;

        let rpc_client = RpcClient::new_with_commitment(
            &config.shared.rpc_url,
            solana_sdk::commitment_config::CommitmentConfig::confirmed(),
        );

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("Failed to create HTTP client: {}", e))?;

        Ok(Self {
            http_client,
            rpc_client,
            signer,
            compute_unit_price_micro_lamports,
            api_url: dflow_config.api_url.clone(),
            api_key: dflow_config.api_key.clone(),
        })
    }

    /// Create a DflowAggregator with direct configuration
    pub fn with_config(
        api_url: String,
        rpc_url: String,
        signer: Arc<Keypair>,
        compute_unit_price_micro_lamports: u64,
        api_key: Option<String>,
    ) -> Result<Self> {
        let rpc_client = RpcClient::new_with_commitment(
            &rpc_url,
            solana_sdk::commitment_config::CommitmentConfig::confirmed(),
        );

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("Failed to create HTTP client: {}", e))?;

        Ok(Self {
            http_client,
            rpc_client,
            signer,
            compute_unit_price_micro_lamports,
            api_url,
            api_key,
        })
    }

    /// Get a quote from DFlow API
    async fn get_quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let url = format!("{}/quote", self.api_url);

        let mut request = self.http_client.get(&url).query(&[
            ("inputMint", input),
            ("outputMint", output),
            ("amount", &amount.to_string()),
            ("slippageBps", &slippage_bps.to_string()),
        ]);

        // Add API key if configured
        if let Some(ref api_key) = self.api_key {
            request = request.header("x-api-key", api_key);
        }

        let response = request
            .send()
            .await
            .map_err(|e| anyhow!("DFlow quote request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!(
                "DFlow quote failed with status {}: {}",
                status,
                error_text
            ));
        }

        // Get response body as text first to help debug parsing errors
        let response_text = response
            .text()
            .await
            .map_err(|e| anyhow!("Failed to read DFlow response body: {}", e))?;

        // Try to parse as JSON
        serde_json::from_str::<QuoteResponse>(&response_text).map_err(|e| {
            anyhow!(
                "Failed to parse DFlow quote response: {}. Response body: {}",
                e,
                response_text
            )
        })
    }

    /// Build a swap transaction from DFlow API
    async fn build_swap_transaction(&self, quote: QuoteResponse) -> Result<VersionedTransaction> {
        let url = format!("{}/swap", self.api_url);

        // Convert QuoteResponse to QuoteResponseForSwap (priceImpactPct as string)
        let quote_for_swap = QuoteResponseForSwap {
            input_mint: quote.input_mint,
            output_mint: quote.output_mint,
            in_amount: quote.in_amount,
            out_amount: quote.out_amount,
            price_impact_pct: quote.price_impact_pct,
            route_plan: quote.route_plan,
            other_amount_threshold: quote.other_amount_threshold,
            min_out_amount: quote.min_out_amount,
            slippage_bps: quote.slippage_bps,
            platform_fee: quote.platform_fee,
            out_transfer_fee: quote.out_transfer_fee,
            context_slot: quote.context_slot,
            simulated_compute_units: quote.simulated_compute_units,
            request_id: quote.request_id,
            for_jito_bundle: quote.for_jito_bundle,
        };

        let swap_request = SwapRequest {
            user_public_key: self.signer.pubkey().to_string(),
            dynamic_compute_unit_limit: None, // Use DFlow's defaults
            prioritization_fee_lamports: if self.compute_unit_price_micro_lamports > 0 {
                Some(self.compute_unit_price_micro_lamports)
            } else {
                None
            },
            quote_response: quote_for_swap,
        };

        let mut request = self.http_client.post(&url).json(&swap_request);

        // Add API key if configured
        if let Some(ref api_key) = self.api_key {
            request = request.header("x-api-key", api_key);
        }

        let response = request
            .send()
            .await
            .map_err(|e| anyhow!("DFlow swap request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!(
                "DFlow swap failed with status {}: {}",
                status,
                error_text
            ));
        }

        let swap_response: SwapResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse DFlow swap response: {}", e))?;

        // Decode base64-encoded transaction
        let tx_bytes = STANDARD
            .decode(&swap_response.swap_transaction)
            .map_err(|e| anyhow!("Failed to decode swap transaction: {}", e))?;

        // Deserialize into VersionedTransaction
        bincode::deserialize::<VersionedTransaction>(&tx_bytes)
            .map_err(|e| anyhow!("Failed to deserialize transaction: {}", e))
    }
}

#[async_trait]
impl DexAggregator for DflowAggregator {
    async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        commitment_level: CommitmentLevel,
        _wrap_and_unwrap_sol: bool,
    ) -> Result<SwapSummary> {
        let start = Instant::now();

        // Validate inputs
        let input_mint = solana_sdk::pubkey::Pubkey::from_str(input)
            .map_err(|e| anyhow!("Invalid input mint: {}", e))?;
        let output_mint = solana_sdk::pubkey::Pubkey::from_str(output)
            .map_err(|e| anyhow!("Invalid output mint: {}", e))?;

        if input_mint == output_mint {
            return Err(anyhow!("Input and output mints must be different"));
        }

        // Get quote
        let quote_start = Instant::now();
        let quote = self.get_quote(input, output, amount, slippage_bps).await?;
        let quote_time = quote_start.elapsed();

        // Parse output amountfrom
        let out_amount: u64 = quote
            .out_amount
            .parse()
            .map_err(|e| anyhow!("Failed to parse output amount: {}", e))?;

        let quote_result = QuoteResult {
            out_amount,
            price_impact: quote.price_impact_pct,
            metadata: QuoteMetadata {
                route: quote.route_plan.as_ref().and_then(|r| {
                    serde_json::to_string(r)
                        .ok()
                        .map(|s| format!("DFlow: {}", s))
                }),
                fees: None, // DFlow doesn't provide separate fee information
                extra: quote.route_plan.clone(),
            },
            quote_time: Some(quote_time),
        };

        // Build transaction
        let mut transaction = self.build_swap_transaction(quote).await?;

        // Sign transaction with user's keypair
        // DFlow returns unsigned transaction, so we need to sign it
        transaction = VersionedTransaction::try_new(transaction.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))?;

        // Submit transaction
        let sig = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &transaction,
                solana_sdk::commitment_config::CommitmentConfig {
                    commitment: commitment_level,
                },
                solana_client::rpc_config::RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

        let execution_time = start.elapsed();

        let swap_result = SwapResult {
            signature: sig.to_string(),
            out_amount,
            slippage_bps_used: Some(slippage_bps),
            aggregator_used: Some(Aggregator::Dflow),
            execution_time: Some(execution_time),
        };

        Ok(SwapSummary {
            swap_result,
            quote_results: vec![(Aggregator::Dflow, quote_result)],
        })
    }

    async fn quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResult> {
        let start = Instant::now();

        // Get quote (quote is just getting a quote without executing)
        let quote = self.get_quote(input, output, amount, slippage_bps).await?;

        // Parse output amount
        let out_amount: u64 = quote
            .out_amount
            .parse()
            .map_err(|e| anyhow!("Failed to parse output amount: {}", e))?;

        let quote_time = start.elapsed();

        Ok(QuoteResult {
            out_amount,
            price_impact: quote.price_impact_pct,
            metadata: QuoteMetadata {
                route: quote.route_plan.as_ref().and_then(|r| {
                    serde_json::to_string(r)
                        .ok()
                        .map(|s| format!("DFlow: {}", s))
                }),
                fees: None, // DFlow doesn't provide separate fee information
                extra: quote.route_plan,
            },
            quote_time: Some(quote_time),
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_dflow_aggregator_creation() {
        // This test would require a valid config and keypair
        // Skipping for now
    }
}

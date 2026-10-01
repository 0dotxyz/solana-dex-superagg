use crate::aggregators::{
    DexAggregator, Quote, QuoteMetadata, QuotePayload, QuoteResult, SwapRequest,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::Instant;

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
    /// Wallet keypair for signing transactions
    signer: Arc<Keypair>,
    /// DFlow API URL
    api_url: String,
    /// Optional API key
    api_key: Option<String>,
}

impl DflowAggregator {
    /// Create a new DflowAggregator from ClientConfig
    pub fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        let dflow_config = config
            .dflow
            .as_ref()
            .ok_or_else(|| anyhow!("DFlow configuration not found"))?;

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("Failed to create HTTP client: {}", e))?;

        Ok(Self {
            http_client,
            signer,
            api_url: dflow_config.api_url.clone(),
            api_key: dflow_config.api_key.clone(),
        })
    }

    /// Get a quote from DFlow API
    async fn get_quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<Value> {
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

        response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse DFlow quote response: {}", e))
    }

    /// Build a swap transaction from DFlow API
    async fn build_transaction_from_quote(
        &self,
        quote: &Value,
        wrap_and_unwrap_sol: bool,
        compute_unit_price_micro_lamports: u64,
    ) -> Result<VersionedTransaction> {
        let url = format!("{}/swap", self.api_url);

        let mut swap_request = json!({
            "userPublicKey": self.signer.pubkey().to_string(),
            "quoteResponse": quote,
            "wrapAndUnwrapSol": wrap_and_unwrap_sol,
            "dynamicComputeUnitLimit": true,
        });
        if compute_unit_price_micro_lamports > 0 {
            swap_request["computeUnitPriceMicroLamports"] =
                json!(compute_unit_price_micro_lamports);
        }

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
    fn kind(&self) -> Aggregator {
        Aggregator::Dflow
    }

    async fn quote(&self, request: &SwapRequest) -> Result<Quote> {
        if request.input_mint == request.output_mint {
            return Err(anyhow!("Input and output mints must be different"));
        }

        let start = Instant::now();
        let quote = self
            .get_quote(
                &request.input_mint.to_string(),
                &request.output_mint.to_string(),
                request.amount,
                request.slippage_bps,
            )
            .await?;

        let out_amount = quote["outAmount"]
            .as_str()
            .and_then(|amount| amount.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("DFlow quote has no valid outAmount: {}", quote))?;
        // Sent as a string, but tolerate a number.
        let price_impact = match &quote["priceImpactPct"] {
            Value::String(pct) => pct.parse().unwrap_or(0.0),
            pct => pct.as_f64().unwrap_or(0.0),
        };
        let route_plan = quote.get("routePlan").cloned();

        Ok(Quote {
            aggregator: Aggregator::Dflow,
            result: QuoteResult {
                out_amount,
                price_impact,
                metadata: QuoteMetadata {
                    route: route_plan.as_ref().map(|r| format!("DFlow: {}", r)),
                    fees: None, // DFlow doesn't provide separate fee information
                    extra: route_plan,
                },
                quote_time: Some(start.elapsed()),
            },
            payload: QuotePayload::Dflow(Box::new(quote)),
        })
    }

    async fn build_transaction(
        &self,
        request: &SwapRequest,
        quote: &Quote,
    ) -> Result<VersionedTransaction> {
        let QuotePayload::Dflow(dflow_quote) = &quote.payload else {
            return Err(anyhow!("DFlow cannot build a {:?} quote", quote.aggregator));
        };
        let transaction = self
            .build_transaction_from_quote(
                dflow_quote,
                request.wrap_and_unwrap_sol,
                request.compute_unit_price_micro_lamports,
            )
            .await?;
        VersionedTransaction::try_new(transaction.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))
    }
}

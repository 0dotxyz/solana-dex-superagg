use crate::aggregators::{
    DexAggregator, Quote, QuoteMetadata, QuotePayload, QuoteResult, SwapRequest,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::Instant;

/// Quote response from DFlow API
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DflowQuote {
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
struct SwapRequestBody {
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    dynamic_compute_unit_limit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_price_micro_lamports: Option<u64>,
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
        Self::with_config(
            dflow_config.api_url.clone(),
            signer,
            dflow_config.api_key.clone(),
        )
    }

    /// Create a DflowAggregator with direct configuration
    pub fn with_config(
        api_url: String,
        signer: Arc<Keypair>,
        api_key: Option<String>,
    ) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("Failed to create HTTP client: {}", e))?;

        Ok(Self {
            http_client,
            signer,
            api_url,
            api_key,
        })
    }

    /// Get a quote from DFlow API
    async fn get_quote(&self, request: &SwapRequest) -> Result<DflowQuote> {
        let url = format!("{}/quote", self.api_url);

        let mut http_request = self.http_client.get(&url).query(&[
            ("inputMint", request.input_mint.to_string()),
            ("outputMint", request.output_mint.to_string()),
            ("amount", request.amount.to_string()),
            ("slippageBps", request.slippage_bps.to_string()),
        ]);
        if let Some(ref api_key) = self.api_key {
            http_request = http_request.header("x-api-key", api_key);
        }

        let response = http_request
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

        // Read the body as text first so parse errors can include it
        let response_text = response
            .text()
            .await
            .map_err(|e| anyhow!("Failed to read DFlow response body: {}", e))?;
        serde_json::from_str::<DflowQuote>(&response_text).map_err(|e| {
            anyhow!(
                "Failed to parse DFlow quote response: {}. Response body: {}",
                e,
                response_text
            )
        })
    }

    /// Build a swap transaction from DFlow API
    async fn build_transaction_from_quote(
        &self,
        request: &SwapRequest,
        quote: DflowQuote,
    ) -> Result<VersionedTransaction> {
        let url = format!("{}/swap", self.api_url);

        // Convert DflowQuote to QuoteResponseForSwap (priceImpactPct as string)
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

        let body = SwapRequestBody {
            user_public_key: self.signer.pubkey().to_string(),
            wrap_and_unwrap_sol: request.wrap_and_unwrap_sol,
            // DFlow simulates and sizes the CU limit, so the priority fee isn't paid on the default.
            dynamic_compute_unit_limit: true,
            // A per-CU price, not a total fee: DFlow's `prioritizationFeeLamports` is a different
            // unit and is left to its default when no price is configured.
            compute_unit_price_micro_lamports: (request.compute_unit_price_micro_lamports > 0)
                .then_some(request.compute_unit_price_micro_lamports),
            quote_response: quote_for_swap,
        };

        let mut http_request = self.http_client.post(&url).json(&body);
        if let Some(ref api_key) = self.api_key {
            http_request = http_request.header("x-api-key", api_key);
        }

        let response = http_request
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

        let tx_bytes = STANDARD
            .decode(&swap_response.swap_transaction)
            .map_err(|e| anyhow!("Failed to decode swap transaction: {}", e))?;
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
        let quote = self.get_quote(request).await?;
        let out_amount: u64 = quote
            .out_amount
            .parse()
            .map_err(|e| anyhow!("Failed to parse output amount: {}", e))?;

        let result = QuoteResult {
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
            quote_time: Some(start.elapsed()),
        };

        Ok(Quote {
            aggregator: Aggregator::Dflow,
            out_amount,
            result,
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
            .build_transaction_from_quote(request, (**dflow_quote).clone())
            .await?;
        VersionedTransaction::try_new(transaction.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))
    }
}

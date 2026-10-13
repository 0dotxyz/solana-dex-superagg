use crate::aggregators::{DexAggregator, Quote, QuotePayload};
use crate::config::{Aggregator, ClientConfig, DflowConfig};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::sync::Arc;

pub(crate) struct DflowAggregator {
    http_client: reqwest::Client,
    api_url: String,
    api_key: String,
    signer: Arc<Keypair>,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: u64,
}

impl DflowAggregator {
    pub fn new(dflow: &DflowConfig, config: &ClientConfig, signer: Arc<Keypair>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            api_url: dflow.api_url.trim_end_matches('/').to_string(),
            api_key: dflow.api_key.clone(),
            signer,
            slippage_bps: config.slippage_bps,
            compute_unit_price_micro_lamports: config.compute_unit_price_micro_lamports,
        }
    }

    async fn call(&self, request: reqwest::RequestBuilder, what: &str) -> Result<Value> {
        let response = request
            .header("x-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| anyhow!("DFlow {} request failed: {}", what, e))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| anyhow!("DFlow {} response unreadable: {}", what, e))?;
        if !status.is_success() {
            return Err(anyhow!(
                "DFlow {} failed with status {}: {}",
                what,
                status,
                body
            ));
        }
        serde_json::from_str(&body).map_err(|e| anyhow!("DFlow {} response invalid: {}", what, e))
    }
}

#[async_trait::async_trait]
impl DexAggregator for DflowAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Dflow
    }

    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote> {
        let quote = self
            .call(
                self.http_client
                    .get(format!("{}/quote", self.api_url))
                    .query(&[
                        ("inputMint", input_mint.to_string()),
                        ("outputMint", output_mint.to_string()),
                        ("amount", amount.to_string()),
                        ("slippageBps", self.slippage_bps.to_string()),
                    ]),
                "quote",
            )
            .await?;
        let out_amount = quote["outAmount"]
            .as_str()
            .and_then(|amount| amount.parse().ok())
            .ok_or_else(|| anyhow!("DFlow quote has no valid outAmount: {}", quote))?;

        Ok(Quote {
            aggregator: Aggregator::Dflow,
            out_amount,
            // Passed back to `/swap` unchanged.
            payload: QuotePayload::Dflow(Box::new(quote)),
        })
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        let QuotePayload::Dflow(quote_response) = &quote.payload else {
            return Err(anyhow!("DFlow cannot build a {:?} quote", quote.aggregator));
        };

        let mut body = json!({
            "userPublicKey": self.signer.pubkey().to_string(),
            "quoteResponse": quote_response,
            "wrapAndUnwrapSol": false,
            // CU limit sized to the simulated route instead of the default.
            "dynamicComputeUnitLimit": true,
        });
        if self.compute_unit_price_micro_lamports > 0 {
            body["computeUnitPriceMicroLamports"] = json!(self.compute_unit_price_micro_lamports);
        }
        let swap = self
            .call(
                self.http_client
                    .post(format!("{}/swap", self.api_url))
                    .json(&body),
                "swap",
            )
            .await?;

        let bytes = swap["swapTransaction"]
            .as_str()
            .and_then(|tx| STANDARD.decode(tx).ok())
            .ok_or_else(|| anyhow!("DFlow swap has no valid swapTransaction"))?;
        let tx = bincode::deserialize::<VersionedTransaction>(&bytes)
            .map_err(|e| anyhow!("Failed to deserialize DFlow transaction: {}", e))?;
        VersionedTransaction::try_new(tx.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign DFlow transaction: {}", e))
    }
}

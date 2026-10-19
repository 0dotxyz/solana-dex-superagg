use crate::aggregators::{DexAggregator, Quote, QuotePayload, Route};
use crate::client::RouteRequest;
use crate::config::{Aggregator, ClientConfig, DflowConfig};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::str::FromStr;
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

    /// Quotes `amount` at `slippage_bps`.
    async fn quote_response(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<Quote> {
        let quote = self
            .call(
                self.http_client
                    .get(format!("{}/quote", self.api_url))
                    .query(&[
                        ("inputMint", input_mint.to_string()),
                        ("outputMint", output_mint.to_string()),
                        ("amount", amount.to_string()),
                        ("slippageBps", slippage_bps.to_string()),
                    ]),
                "quote",
            )
            .await?;
        let out_amount = parse_u64(&quote["outAmount"])
            .ok_or_else(|| anyhow!("DFlow quote has no valid outAmount: {}", quote))?;
        let min_out_amount = parse_u64(&quote["otherAmountThreshold"])
            .ok_or_else(|| anyhow!("DFlow quote has no valid otherAmountThreshold: {}", quote))?;

        Ok(Quote {
            aggregator: Aggregator::Dflow,
            out_amount,
            min_out_amount,
            // Passed back to `/swap` unchanged.
            payload: QuotePayload::Dflow(Box::new(quote)),
        })
    }

    /// Posts the quote to `/swap` or `/swap-instructions` (same body) on behalf of `user`.
    /// `dynamic_compute_unit_limit` makes DFlow simulate the swap against the wallet's current
    /// balances. That is wrong for instructions: their input may only arrive earlier in the
    /// caller's transaction.
    async fn post_swap(
        &self,
        endpoint: &str,
        quote: &Quote,
        user: &Pubkey,
        dynamic_compute_unit_limit: bool,
    ) -> Result<Value> {
        let QuotePayload::Dflow(quote_response) = &quote.payload else {
            return Err(anyhow!("DFlow cannot build a {:?} quote", quote.aggregator));
        };

        let mut body = json!({
            "userPublicKey": user.to_string(),
            "quoteResponse": quote_response,
            "wrapAndUnwrapSol": false,
            "dynamicComputeUnitLimit": dynamic_compute_unit_limit,
        });
        if self.compute_unit_price_micro_lamports > 0 {
            body["computeUnitPriceMicroLamports"] = json!(self.compute_unit_price_micro_lamports);
        }
        self.call(
            self.http_client
                .post(format!("{}/{}", self.api_url, endpoint))
                .json(&body),
            endpoint,
        )
        .await
    }
}

fn parse_u64(value: &Value) -> Option<u64> {
    value.as_str().and_then(|amount| amount.parse().ok())
}

fn parse_bool(value: &Value) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| anyhow!("DFlow: invalid bool {}", value))
}

fn parse_pubkey(value: &Value) -> Result<Pubkey> {
    value
        .as_str()
        .and_then(|key| Pubkey::from_str(key).ok())
        .ok_or_else(|| anyhow!("DFlow: invalid pubkey {}", value))
}

fn parse_instruction(value: &Value) -> Result<Instruction> {
    let accounts = value["accounts"]
        .as_array()
        .ok_or_else(|| anyhow!("DFlow: instruction without accounts: {}", value))?
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
            .ok_or_else(|| anyhow!("DFlow: instruction data is not base64: {}", value))?,
    })
}

#[async_trait::async_trait]
impl DexAggregator for DflowAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Dflow
    }

    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote> {
        self.quote_response(input_mint, output_mint, amount, self.slippage_bps)
            .await
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        // CU limit sized to the simulated route instead of the default.
        let swap = self
            .post_swap("swap", quote, &self.signer.pubkey(), true)
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

    async fn quote_route(&self, request: &RouteRequest, amount: u64) -> Result<Quote> {
        self.quote_response(
            &request.input_mint,
            &request.output_mint,
            amount,
            request.slippage_bps,
        )
        .await
    }

    async fn build_instructions(&self, request: &RouteRequest, quote: &Quote) -> Result<Route> {
        let response = self
            .post_swap("swap-instructions", quote, &request.user, false)
            .await?;
        // We never ask for wSOL unwrapping (cleanup), so it or "other" instructions showing up
        // means a response we don't understand. Setup instructions create token accounts the
        // route needs.
        for key in ["cleanupInstructions", "otherInstructions"] {
            if response[key]
                .as_array()
                .is_some_and(|list| !list.is_empty())
            {
                return Err(anyhow!("DFlow returned {} that are not handed out", key));
            }
        }
        let mut instructions = response["setupInstructions"]
            .as_array()
            .map(|setup| {
                setup
                    .iter()
                    .map(parse_instruction)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        instructions.push(parse_instruction(&response["swapInstruction"])?);
        let lookup_tables = response["addressLookupTableAddresses"]
            .as_array()
            .map(|keys| keys.iter().map(parse_pubkey).collect::<Result<Vec<_>>>())
            .transpose()?
            .unwrap_or_default();

        Ok(Route {
            instructions,
            address_lookup_tables: lookup_tables,
        })
    }
}

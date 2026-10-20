use crate::aggregators::{sign_transaction, DexAggregator, Quote, QuotePayload, Route};
use crate::client::RouteRequest;
use crate::config::{Aggregator, ClientConfig, DflowConfig};
use crate::json::{parse_instruction, parse_pubkey, parse_u64};
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
    compute_unit_price_micro_lamports: u64,
}

impl DflowAggregator {
    pub fn new(dflow: &DflowConfig, config: &ClientConfig, signer: Arc<Keypair>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            api_url: dflow.api_url.trim_end_matches('/').to_string(),
            api_key: dflow.api_key.clone(),
            signer,
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
        let body = swap_body(
            quote.dflow()?,
            user,
            dynamic_compute_unit_limit,
            self.compute_unit_price_micro_lamports,
        );
        self.call(
            self.http_client
                .post(format!("{}/{}", self.api_url, endpoint))
                .json(&body),
            endpoint,
        )
        .await
    }
}

fn swap_body(
    quote: &Value,
    user: &Pubkey,
    dynamic_compute_unit_limit: bool,
    compute_unit_price_micro_lamports: u64,
) -> Value {
    let mut body = json!({
        "userPublicKey": user.to_string(),
        "quoteResponse": quote,
        "wrapAndUnwrapSol": false,
        "dynamicComputeUnitLimit": dynamic_compute_unit_limit,
    });
    if compute_unit_price_micro_lamports > 0 {
        body["computeUnitPriceMicroLamports"] = json!(compute_unit_price_micro_lamports);
    }
    body
}

/// The serialized transaction in a `/swap` response.
fn transaction_bytes(swap: &Value) -> Result<Vec<u8>> {
    swap["swapTransaction"]
        .as_str()
        .and_then(|tx| STANDARD.decode(tx).ok())
        .ok_or_else(|| anyhow!("DFlow swap has no valid swapTransaction"))
}

/// The route in a `/swap-instructions` response. We never ask for wSOL unwrapping (cleanup), so it
/// or "other" instructions showing up means a response we don't understand. Setup instructions
/// create token accounts the route needs.
fn route(response: &Value) -> Result<Route> {
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
    let address_lookup_tables = response["addressLookupTableAddresses"]
        .as_array()
        .map(|keys| keys.iter().map(parse_pubkey).collect::<Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();

    Ok(Route {
        instructions,
        address_lookup_tables,
    })
}

#[async_trait::async_trait]
impl DexAggregator for DflowAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Dflow
    }

    async fn quote(&self, request: &RouteRequest, amount: u64) -> Result<Quote> {
        let quote = self
            .call(
                self.http_client
                    .get(format!("{}/quote", self.api_url))
                    .query(&[
                        ("inputMint", request.input_mint.to_string()),
                        ("outputMint", request.output_mint.to_string()),
                        ("amount", amount.to_string()),
                        ("slippageBps", request.slippage_bps.to_string()),
                    ]),
                "quote",
            )
            .await?;
        let min_out_amount = parse_u64(&quote["otherAmountThreshold"])
            .map_err(|e| anyhow!("DFlow quote otherAmountThreshold: {}", e))?;

        Ok(Quote {
            aggregator: Aggregator::Dflow,
            min_out_amount,
            // Passed back to `/swap` unchanged.
            payload: QuotePayload::Dflow(Box::new(quote)),
        })
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        // CU limit sized to the simulated route instead of the default.
        let swap = self
            .post_swap("swap", quote, &self.signer.pubkey(), true)
            .await?;
        sign_transaction(&transaction_bytes(&swap)?, &self.signer, Aggregator::Dflow)
    }

    async fn build_instructions(&self, request: &RouteRequest, quote: &Quote) -> Result<Route> {
        let response = self
            .post_swap("swap-instructions", quote, &request.user, false)
            .await?;
        route(&response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instruction_json(program: &Pubkey) -> Value {
        json!({ "programId": program.to_string(), "accounts": [], "data": "" })
    }

    #[test]
    fn the_swap_body_carries_the_quote_and_our_settings() {
        let user = Pubkey::new_unique();
        let quote = json!({ "inAmount": "1", "routePlan": [] });
        let body = swap_body(&quote, &user, false, 0);
        assert_eq!(body["userPublicKey"], user.to_string());
        assert_eq!(body["quoteResponse"], quote);
        assert_eq!(body["wrapAndUnwrapSol"], false);
        assert_eq!(body["dynamicComputeUnitLimit"], false);
        assert!(body.get("computeUnitPriceMicroLamports").is_none());

        let body = swap_body(&quote, &user, true, 5_000);
        assert_eq!(body["dynamicComputeUnitLimit"], true);
        assert_eq!(body["computeUnitPriceMicroLamports"], 5_000);
    }

    #[test]
    fn the_route_is_the_setup_and_swap_instructions_with_its_lookup_tables() {
        let (ata, router, table) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let response = json!({
            "setupInstructions": [instruction_json(&ata)],
            "swapInstruction": instruction_json(&router),
            "cleanupInstructions": [],
            "addressLookupTableAddresses": [table.to_string()],
        });
        let built = route(&response).unwrap();
        let programs: Vec<_> = built.instructions.iter().map(|ix| ix.program_id).collect();
        assert_eq!(programs, [ata, router]);
        assert_eq!(built.address_lookup_tables, [table]);

        let bare = route(&json!({ "swapInstruction": instruction_json(&router) })).unwrap();
        assert_eq!(bare.instructions.len(), 1);
        assert!(bare.address_lookup_tables.is_empty());
    }

    #[test]
    fn instructions_we_did_not_ask_for_are_an_error() {
        let router = Pubkey::new_unique();
        for key in ["cleanupInstructions", "otherInstructions"] {
            let response = json!({
                "swapInstruction": instruction_json(&router),
                key: [instruction_json(&router)],
            });
            let error = route(&response).unwrap_err().to_string();
            assert!(error.contains(key), "{}", error);
        }
        assert!(route(&json!({})).is_err());
    }

    #[test]
    fn the_swap_transaction_is_base64() {
        let swap = json!({ "swapTransaction": STANDARD.encode([1, 2, 3]) });
        assert_eq!(transaction_bytes(&swap).unwrap(), [1, 2, 3]);
        assert!(transaction_bytes(&json!({ "swapTransaction": "%%%" })).is_err());
        assert!(transaction_bytes(&json!({})).is_err());
    }
}

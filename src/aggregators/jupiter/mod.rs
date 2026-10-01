//! Jupiter aggregator implementation

use crate::aggregators::{
    DexAggregator, Quote, QuoteMetadata, QuotePayload, QuoteResult, SwapRequest,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest as JupiterSwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::Instant;

pub struct JupiterAggregator {
    jupiter_client: JupiterSwapApiClient,
    signer: Arc<Keypair>,
}

impl JupiterAggregator {
    pub fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        let api_key = config
            .jupiter
            .api_key
            .clone()
            .ok_or_else(|| anyhow!("Jupiter API key is not configured"))?;
        let jupiter_client =
            JupiterSwapApiClient::new(config.jupiter.jup_swap_api_url.clone(), api_key)?;
        Ok(Self {
            jupiter_client,
            signer,
        })
    }
}

#[async_trait::async_trait]
impl DexAggregator for JupiterAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Jupiter
    }

    async fn quote(&self, request: &SwapRequest) -> Result<Quote> {
        if request.input_mint == request.output_mint {
            return Err(anyhow!("Input and output mints cannot be the same"));
        }

        let quote_start = Instant::now();
        let quote_response = self
            .jupiter_client
            .quote(&QuoteRequest {
                input_mint: request.input_mint.to_bytes().into(),
                output_mint: request.output_mint.to_bytes().into(),
                amount: request.amount,
                slippage_bps: request.slippage_bps,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("Jupiter quote failed: {}", e))?;

        let result = QuoteResult {
            out_amount: quote_response.out_amount,
            price_impact: quote_response
                .price_impact_pct
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0),
            metadata: QuoteMetadata {
                route: None,
                fees: quote_response.platform_fee.as_ref().map(|f| f.amount),
                extra: None,
            },
            quote_time: Some(quote_start.elapsed()),
        };

        Ok(Quote {
            aggregator: Aggregator::Jupiter,
            result,
            payload: QuotePayload::Jupiter(Box::new(quote_response)),
        })
    }

    async fn build_transaction(
        &self,
        request: &SwapRequest,
        quote: &Quote,
    ) -> Result<VersionedTransaction> {
        let QuotePayload::Jupiter(quote_response) = &quote.payload else {
            return Err(anyhow!(
                "Jupiter cannot build a {:?} quote",
                quote.aggregator
            ));
        };

        let swap_response = self
            .jupiter_client
            .swap(
                &JupiterSwapRequest {
                    user_public_key: self.signer.pubkey().to_bytes().into(),
                    quote_response: (**quote_response).clone(),
                    config: TransactionConfig {
                        wrap_and_unwrap_sol: request.wrap_and_unwrap_sol,
                        // CU limit sized to the simulated route instead of the 1.4M default.
                        dynamic_compute_unit_limit: true,
                        compute_unit_price_micro_lamports: (request
                            .compute_unit_price_micro_lamports
                            > 0)
                        .then_some(ComputeUnitPriceMicroLamports::MicroLamports(
                            request.compute_unit_price_micro_lamports,
                        )),
                        ..Default::default()
                    },
                },
                None,
            )
            .await
            .map_err(|e| anyhow!("Jupiter swap failed: {}", e))?;

        if let Some(error) = swap_response.simulation_error {
            return Err(anyhow!("Jupiter swap simulation failed: {:?}", error));
        }

        let tx = bincode::deserialize::<VersionedTransaction>(&swap_response.swap_transaction)
            .map_err(|e| anyhow!("Failed to deserialize Jupiter transaction: {}", e))?;
        VersionedTransaction::try_new(tx.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign Jupiter transaction: {}", e))
    }
}

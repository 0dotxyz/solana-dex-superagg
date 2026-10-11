use crate::aggregators::{DexAggregator, Quote, QuotePayload};
use crate::config::{Aggregator, ClientConfig, JupiterConfig};
use anyhow::{anyhow, Result};
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;

pub(crate) struct JupiterAggregator {
    jupiter_client: JupiterSwapApiClient,
    signer: Arc<Keypair>,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: u64,
}

impl JupiterAggregator {
    pub fn new(
        jupiter: &JupiterConfig,
        config: &ClientConfig,
        signer: Arc<Keypair>,
    ) -> Result<Self> {
        Ok(Self {
            jupiter_client: JupiterSwapApiClient::new(
                jupiter.jup_swap_api_url.clone(),
                jupiter.api_key.clone(),
            )?,
            signer,
            slippage_bps: config.slippage_bps,
            compute_unit_price_micro_lamports: config.compute_unit_price_micro_lamports,
        })
    }
}

#[async_trait::async_trait]
impl DexAggregator for JupiterAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Jupiter
    }

    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote> {
        let quote_response = self
            .jupiter_client
            .quote(&QuoteRequest {
                input_mint: input_mint.to_bytes().into(),
                output_mint: output_mint.to_bytes().into(),
                amount,
                slippage_bps: self.slippage_bps,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("Jupiter quote failed: {}", e))?;

        Ok(Quote {
            aggregator: Aggregator::Jupiter,
            out_amount: quote_response.out_amount,
            payload: QuotePayload::Jupiter(Box::new(quote_response)),
        })
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        let QuotePayload::Jupiter(quote_response) = &quote.payload else {
            return Err(anyhow!(
                "Jupiter cannot build a {:?} quote",
                quote.aggregator
            ));
        };

        let swap_response = self
            .jupiter_client
            .swap(
                &SwapRequest {
                    user_public_key: self.signer.pubkey().to_bytes().into(),
                    quote_response: (**quote_response).clone(),
                    config: TransactionConfig {
                        wrap_and_unwrap_sol: false,
                        // CU limit sized to the simulated route instead of the 1.4M default.
                        dynamic_compute_unit_limit: true,
                        compute_unit_price_micro_lamports: (self.compute_unit_price_micro_lamports
                            > 0)
                        .then_some(ComputeUnitPriceMicroLamports::MicroLamports(
                            self.compute_unit_price_micro_lamports,
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

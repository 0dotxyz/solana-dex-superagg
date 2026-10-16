use crate::aggregators::{DexAggregator, Quote, QuotePayload};
use crate::config::{Aggregator, ClientConfig, JupiterConfig};
use anyhow::{anyhow, Result};
use jupiter_swap_api_client::{
    quote::{QuoteRequest, QuoteResponse},
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
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

    /// `dynamic_compute_unit_limit` makes Jupiter simulate the swap against the wallet's current
    /// balances, so it is off for instructions, whose input may only arrive earlier in the
    /// caller's transaction.
    fn swap_request(&self, quote: &Quote, dynamic_compute_unit_limit: bool) -> Result<SwapRequest> {
        let QuotePayload::Jupiter(quote_response) = &quote.payload else {
            return Err(anyhow!(
                "Jupiter cannot build a {:?} quote",
                quote.aggregator
            ));
        };
        Ok(SwapRequest {
            user_public_key: self.signer.pubkey().to_bytes().into(),
            quote_response: QuoteResponse::clone(quote_response),
            config: TransactionConfig {
                wrap_and_unwrap_sol: false,
                dynamic_compute_unit_limit,
                compute_unit_price_micro_lamports: (self.compute_unit_price_micro_lamports > 0)
                    .then_some(ComputeUnitPriceMicroLamports::MicroLamports(
                        self.compute_unit_price_micro_lamports,
                    )),
                ..Default::default()
            },
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
            min_out_amount: quote_response.other_amount_threshold,
            payload: QuotePayload::Jupiter(Box::new(quote_response)),
        })
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        let swap_response = self
            .jupiter_client
            // CU limit sized to the simulated route instead of the 1.4M default.
            .swap(&self.swap_request(quote, true)?, None)
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

    async fn build_instructions(&self, quote: &Quote) -> Result<(Vec<Instruction>, Vec<Pubkey>)> {
        let response = self
            .jupiter_client
            .swap_instructions(&self.swap_request(quote, false)?)
            .await
            .map_err(|e| anyhow!("Jupiter swap-instructions failed: {}", e))?;

        // Cleanup unwraps wSOL and "other" is a Jito tip; neither is requested, so either showing
        // up means the response is not what we embed.
        if response.cleanup_instruction.is_some() || !response.other_instructions.is_empty() {
            return Err(anyhow!(
                "Jupiter returned cleanup/other instructions that are not embedded"
            ));
        }
        let mut instructions = response.setup_instructions;
        instructions.push(response.swap_instruction);
        Ok((instructions, response.address_lookup_table_addresses))
    }
}

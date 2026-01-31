//! Jupiter aggregator implementation

use crate::aggregators::{DexAggregator, QuoteMetadata, QuoteResult, SwapResult, SwapSummary};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::time::Instant;
use std::{str::FromStr, sync::Arc};

pub struct JupiterAggregator {
    jupiter_client: JupiterSwapApiClient,
    rpc_client: RpcClient,
    signer: Arc<Keypair>,
    compute_unit_price_micro_lamports: u64,
}

impl JupiterAggregator {
    pub fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        Self::new_with_compute_price(
            config,
            signer,
            config.shared.compute_unit_price_micro_lamports,
        )
    }

    pub fn new_with_compute_price(
        config: &ClientConfig,
        signer: Arc<Keypair>,
        compute_unit_price_micro_lamports: u64,
    ) -> Result<Self> {
        let rpc_client = RpcClient::new_with_commitment(
            &config.shared.rpc_url,
            solana_sdk::commitment_config::CommitmentConfig::confirmed(),
        );

        let api_key = config
            .jupiter
            .api_key
            .clone()
            .ok_or_else(|| anyhow!("Jupiter API key is not configured"))?;
        let jupiter_client =
            JupiterSwapApiClient::new(config.jupiter.jup_swap_api_url.clone(), api_key)?;

        Ok(Self {
            jupiter_client,
            rpc_client,
            signer,
            compute_unit_price_micro_lamports,
        })
    }

    pub fn with_config(
        jupiter_api_url: &str,
        rpc_url: &str,
        signer: Arc<Keypair>,
        compute_unit_price_micro_lamports: u64,
        api_key: Option<String>,
    ) -> Result<Self> {
        let rpc_client = RpcClient::new_with_commitment(
            rpc_url,
            solana_sdk::commitment_config::CommitmentConfig::confirmed(),
        );
        let jupiter_client = JupiterSwapApiClient::new(
            jupiter_api_url.to_string(),
            api_key.ok_or_else(|| anyhow!("Jupiter API key is required when using with_config"))?,
        )?;

        Ok(Self {
            jupiter_client,
            rpc_client,
            signer,
            compute_unit_price_micro_lamports,
        })
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
        commitment_level: solana_sdk::commitment_config::CommitmentLevel,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapSummary> {
        let start_time = Instant::now();

        let input_mint =
            Pubkey::from_str(input).map_err(|e| anyhow!("Invalid input mint: {}", e))?;
        let output_mint =
            Pubkey::from_str(output).map_err(|e| anyhow!("Invalid output mint: {}", e))?;

        if input_mint == output_mint {
            return Err(anyhow!("Input and output mints cannot be the same"));
        }

        let quote_start = Instant::now();
        let quote_response = self
            .jupiter_client
            .quote(&QuoteRequest {
                input_mint,
                output_mint,
                amount,
                slippage_bps,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("Jupiter quote failed: {}", e))?;
        let quote_time = quote_start.elapsed();

        let out_amount = quote_response.out_amount;
        let price_impact = quote_response
            .price_impact_pct
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0);
        let quote_result = QuoteResult {
            out_amount,
            price_impact,
            metadata: QuoteMetadata {
                route: None,
                fees: quote_response.platform_fee.as_ref().map(|f| f.amount),
                extra: None,
            },
            quote_time: Some(quote_time),
        };

        let swap_config = TransactionConfig {
            wrap_and_unwrap_sol,
            compute_unit_price_micro_lamports: if self.compute_unit_price_micro_lamports > 0 {
                Some(ComputeUnitPriceMicroLamports::MicroLamports(
                    self.compute_unit_price_micro_lamports,
                ))
            } else {
                None
            },
            ..Default::default()
        };

        let user_pubkey = self.signer.pubkey();

        let swap_response = self
            .jupiter_client
            .swap(
                &SwapRequest {
                    user_public_key: user_pubkey,
                    quote_response,
                    config: swap_config,
                },
                None,
            )
            .await
            .map_err(|e| anyhow!("Jupiter swap failed: {}", e))?;

        let mut tx = bincode::deserialize::<VersionedTransaction>(&swap_response.swap_transaction)
            .map_err(|e| anyhow!("Failed to deserialize transaction: {}", e))?;

        tx = VersionedTransaction::try_new(tx.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))?;

        let sig = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                solana_sdk::commitment_config::CommitmentConfig {
                    commitment: commitment_level,
                },
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(
                        solana_sdk::commitment_config::CommitmentLevel::Processed,
                    ),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

        let execution_time = start_time.elapsed();

        let swap_result = SwapResult {
            signature: sig.to_string(),
            out_amount,
            slippage_bps_used: Some(slippage_bps),
            aggregator_used: Some(Aggregator::Jupiter),
            execution_time: Some(execution_time),
        };

        Ok(SwapSummary {
            swap_result,
            quote_results: vec![(Aggregator::Jupiter, quote_result)],
        })
    }

    async fn quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResult> {
        let start_time = Instant::now();

        let input_mint =
            Pubkey::from_str(input).map_err(|e| anyhow!("Invalid input mint: {}", e))?;
        let output_mint =
            Pubkey::from_str(output).map_err(|e| anyhow!("Invalid output mint: {}", e))?;

        if input_mint == output_mint {
            return Err(anyhow!("Input and output mints cannot be the same"));
        }

        let quote_response = self
            .jupiter_client
            .quote(&QuoteRequest {
                input_mint,
                output_mint,
                amount,
                slippage_bps,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("Jupiter quote failed: {}", e))?;

        let quote_time = start_time.elapsed();

        // Convert Decimal to f64 using TryInto
        let price_impact = quote_response
            .price_impact_pct
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0);

        Ok(QuoteResult {
            out_amount: quote_response.out_amount,
            price_impact,
            metadata: QuoteMetadata {
                route: None,
                fees: quote_response.platform_fee.as_ref().map(|f| f.amount),
                extra: None,
            },
            quote_time: Some(quote_time),
        })
    }
}

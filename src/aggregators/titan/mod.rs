//! Titan aggregator implementation
//!
//! This module implements the `DexAggregator` trait for Titan protocol.

mod client;
mod codec;
mod transaction_builder;
mod types;

use crate::aggregators::{DexAggregator, QuoteMetadata, SimulateResult, SwapResult};
use crate::config::ClientConfig;
use anyhow::{anyhow, Result};
use solana_client::{
    nonblocking::rpc_client::RpcClient as AsyncRpcClient, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::Instant;

pub use client::TitanClient;
pub use types::SwapRoute;

use self::transaction_builder::build_transaction_from_route;

/// Titan aggregator implementation
pub struct TitanAggregator {
    /// Titan WebSocket client (keeps connection open)
    titan_client: TitanClient,
    /// Solana RPC client for transaction submission
    rpc_client: RpcClient,
    /// Async RPC client for blockhash fetching
    async_rpc_client: AsyncRpcClient,
    /// Wallet keypair for signing transactions
    signer: Arc<Keypair>,
    /// RPC URL for transaction building
    rpc_url: String,
}

impl TitanAggregator {
    /// Create a new Titan aggregator from ClientConfig
    pub async fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        let titan_config = config
            .titan
            .as_ref()
            .ok_or_else(|| anyhow!("Titan configuration not provided"))?;

        // Determine Titan endpoint configuration
        let titan_api_key = titan_config
            .titan_api_key
            .as_ref()
            .ok_or_else(|| anyhow!("Titan API key must be provided"))?;

        if titan_api_key.is_empty() {
            return Err(anyhow!("TITAN_API_KEY is set but empty"));
        }

        let titan_ws_endpoint = titan_config.titan_ws_endpoint.clone();

        // Create RPC clients
        let rpc_client = RpcClient::new(&config.shared.rpc_url);
        let async_rpc_client = AsyncRpcClient::new(config.shared.rpc_url.clone());

        // Create Titan client and connect
        let titan_client = TitanClient::new(titan_ws_endpoint, titan_api_key.clone());
        titan_client
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to Titan: {}", e))?;

        Ok(Self {
            titan_client,
            rpc_client,
            async_rpc_client,
            signer,
            rpc_url: config.shared.rpc_url.clone(),
        })
    }

    /// Create a new Titan aggregator with explicit configuration
    pub async fn with_config(
        titan_ws_endpoint: String,
        titan_api_key: Option<String>,
        rpc_url: String,
        signer: Arc<Keypair>,
    ) -> Result<Self> {
        // Determine Titan endpoint configuration
        let api_key = titan_api_key.ok_or_else(|| anyhow!("Titan API key must be provided"))?;

        if api_key.is_empty() {
            return Err(anyhow!("TITAN_API_KEY is set but empty"));
        }

        let ws_endpoint = titan_ws_endpoint;

        // Create RPC clients
        let rpc_client = RpcClient::new(&rpc_url);
        let async_rpc_client = AsyncRpcClient::new(rpc_url.clone());

        // Create Titan client and connect
        let titan_client = TitanClient::new(ws_endpoint, api_key);
        titan_client
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to Titan: {}", e))?;

        Ok(Self {
            titan_client,
            rpc_client,
            async_rpc_client,
            signer,
            rpc_url,
        })
    }

    /// Close the WebSocket connection
    ///
    /// Call this when you're done with the aggregator to clean up resources.
    pub async fn close(&self) -> Result<()> {
        self.titan_client
            .close()
            .await
            .map_err(|e| anyhow!("Failed to close Titan connection: {}", e))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl DexAggregator for TitanAggregator {
    async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        commitment_level: solana_sdk::commitment_config::CommitmentLevel,
    ) -> Result<SwapResult> {
        let start_time = Instant::now();

        let user_pubkey = self.signer.as_ref().pubkey().to_string();

        // Request swap quotes with slippage
        let (_provider, route) = self
            .titan_client
            .request_swap_quotes(input, output, amount, &user_pubkey, Some(slippage_bps))
            .await
            .map_err(|e| anyhow!("Failed to get swap quotes: {}", e))?;

        // Get recent blockhash
        let recent_blockhash = self
            .async_rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|e| anyhow!("Failed to get blockhash: {}", e))?;

        // Build transaction
        let transaction_bytes = build_transaction_from_route(
            &route,
            self.signer.as_ref().pubkey(),
            recent_blockhash,
            &self.rpc_url,
            None, // No Jito tip for now
        )
        .await
        .map_err(|e| anyhow!("Failed to build transaction: {}", e))?;

        // Deserialize transaction
        let mut transaction: VersionedTransaction = bincode::deserialize(&transaction_bytes)
            .map_err(|e| anyhow!("Failed to deserialize transaction: {}", e))?;

        // Sign transaction
        let message = transaction.message.serialize();
        let signature = self.signer.as_ref().sign_message(&message);
        transaction.signatures[0] = signature;

        // Send transaction with specified commitment level
        let commitment_config = CommitmentConfig {
            commitment: commitment_level,
        };
        let tx_signature = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &transaction,
                commitment_config,
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

        Ok(SwapResult {
            signature: tx_signature.to_string(),
            out_amount: route.out_amount,
            slippage_bps_used: Some(slippage_bps),
            aggregator_used: Some(crate::config::Aggregator::Titan),
            execution_time: Some(execution_time),
        })
    }

    async fn simulate(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<SimulateResult> {
        let start_time = Instant::now();

        let user_pubkey = self.signer.as_ref().pubkey().to_string();

        // Request swap quotes with slippage (simulation doesn't execute)
        let (_provider, route) = self
            .titan_client
            .request_swap_quotes(input, output, amount, &user_pubkey, Some(slippage_bps))
            .await
            .map_err(|e| anyhow!("Failed to get swap quotes: {}", e))?;

        let sim_time = start_time.elapsed();

        // Calculate price impact (simplified: compare in_amount vs out_amount)
        // This is a rough estimate - actual price impact would require more market data
        let price_impact = if route.in_amount > 0 {
            let ratio = route.out_amount as f64 / route.in_amount as f64;
            // Assuming 1:1 would be no impact, calculate deviation
            // This is a simplified calculation
            (1.0 - ratio).abs() * 100.0
        } else {
            0.0
        };

        Ok(SimulateResult {
            out_amount: route.out_amount,
            price_impact,
            metadata: QuoteMetadata {
                route: Some(format!("Titan route with {} steps", route.steps.len())),
                fees: route.platform_fee.as_ref().map(|f| f.amount),
                extra: None,
            },
            sim_time: Some(sim_time),
        })
    }
}

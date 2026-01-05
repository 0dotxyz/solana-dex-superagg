use crate::aggregators::{
    jupiter::JupiterAggregator, titan::TitanAggregator, DexAggregator, SwapResult,
};
use crate::config::{Aggregator, ClientConfig, RouteConfig, RoutingStrategy};
use anyhow::{anyhow, Result};
use solana_client::{rpc_client::RpcClient, rpc_config::CommitmentConfig};
use solana_sdk::signature::{Keypair, Signer};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Main client for routing orders across multiple DEX aggregators (Jupiter, Titan, etc.)
pub struct DexSuperAggClient {
    /// Wallet keypair for signing transactions
    signer: Arc<Keypair>,
    /// Solana RPC client
    rpc_client: RpcClient,
    /// Client configuration
    config: ClientConfig,
    /// Cached Titan aggregator (lazily initialized, reused across swaps)
    /// This prevents opening a new WebSocket connection for each swap
    titan_aggregator: Arc<Mutex<Option<Arc<TitanAggregator>>>>,
}

impl DexSuperAggClient {
    /// Create a new DexSuperAggClient from configuration
    /// Requires a wallet keypair to be configured
    pub fn new(config: ClientConfig) -> Result<Self> {
        let signer = config
            .get_keypair()?
            .ok_or_else(|| anyhow::anyhow!("Wallet keypair is required for DexSuperAggClient"))?;

        let rpc_client =
            RpcClient::new_with_commitment(&config.shared.rpc_url, CommitmentConfig::confirmed());

        Ok(Self {
            signer: Arc::new(signer),
            rpc_client,
            config,
            titan_aggregator: Arc::new(Mutex::new(None)),
        })
    }

    /// Create a new DexSuperAggClient from environment variables
    pub fn from_env() -> Result<Self> {
        let config = ClientConfig::from_env()?;
        Self::new(config)
    }

    /// Get the signer's public key
    pub fn pubkey(&self) -> solana_sdk::pubkey::Pubkey {
        self.signer.pubkey()
    }

    /// Get a reference to the configuration
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Get a reference to the RPC client
    pub fn rpc_client(&self) -> &RpcClient {
        &self.rpc_client
    }

    /// Get a reference to the signer
    pub fn signer(&self) -> &Arc<Keypair> {
        &self.signer
    }

    /// Get or create Titan aggregator (reuses WebSocket connection)
    /// This method lazily initializes the Titan aggregator and reuses it across swaps
    async fn get_titan_aggregator(&self) -> Result<Arc<TitanAggregator>> {
        let mut titan_opt = self.titan_aggregator.lock().await;

        if titan_opt.is_none() {
            // Initialize Titan aggregator if not already created
            let titan = TitanAggregator::new(&self.config, Arc::clone(&self.signer)).await?;
            *titan_opt = Some(Arc::new(titan));
        }

        // Clone the Arc to return a reference
        Ok(titan_opt.as_ref().unwrap().clone())
    }

    /// Close the Titan WebSocket connection
    /// Call this when you're done with the client to clean up resources
    pub async fn close(&self) -> Result<()> {
        let mut titan_opt = self.titan_aggregator.lock().await;
        if let Some(titan) = titan_opt.take() {
            titan.close().await?;
        }
        Ok(())
    }

    /// Execute a swap using default routing configuration
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    ///
    /// # Returns
    /// `SwapResult` containing the transaction signature and output amount
    ///
    /// Uses the default routing strategy from the client configuration.
    pub async fn swap(&self, input: &str, output: &str, amount: u64) -> Result<SwapResult> {
        let route_config = self.config.default_route_config();
        self.swap_with_route_config(input, output, amount, route_config)
            .await
    }

    /// Execute a swap with a custom route configuration
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    /// * `route_config` - Route configuration for this swap
    ///
    /// # Returns
    /// `SwapResult` containing the transaction signature and output amount
    ///
    /// The route configuration allows per-swap customization of:
    /// - Compute unit price
    /// - Routing strategy (which aggregator to use)
    /// - Retry behavior
    pub async fn swap_with_route_config(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: RouteConfig,
    ) -> Result<SwapResult> {
        let slippage_bps = self.config.shared.slippage_bps;

        // Determine which aggregator(s) to use based on routing strategy
        match &route_config.routing_strategy {
            Some(RoutingStrategy::BestPrice) | None => {
                // Compare available aggregators and use the one with best price (most tokens)
                self.swap_best_price(input, output, amount, slippage_bps, &route_config)
                    .await
            }
            Some(RoutingStrategy::PreferredAggregator {
                aggregator,
                simulate,
            }) => {
                if *simulate {
                    // Simulate first, then execute
                    self.swap_with_simulation(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        aggregator,
                        &route_config,
                    )
                    .await
                } else {
                    // Direct swap without simulation
                    self.swap_direct(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        aggregator,
                        &route_config,
                    )
                    .await
                }
            }
            Some(RoutingStrategy::LowestSlippageClimber {
                floor_slippage_bps,
                max_slippage_bps,
                step_bps,
            }) => {
                self.swap_lowest_slippage_climber(
                    input,
                    output,
                    amount,
                    *floor_slippage_bps,
                    *max_slippage_bps,
                    *step_bps,
                    &route_config,
                )
                .await
            }
        }
    }

    /// Execute a direct swap with a specific aggregator
    async fn swap_direct(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        aggregator: &Aggregator,
        route_config: &RouteConfig,
    ) -> Result<SwapResult> {
        match aggregator {
            Aggregator::Jupiter => {
                let jupiter = JupiterAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                jupiter.swap(input, output, amount, slippage_bps).await
            }
            Aggregator::Titan => {
                // Reuse existing Titan aggregator to avoid opening new WebSocket connections
                let titan = self.get_titan_aggregator().await?;
                titan.swap(input, output, amount, slippage_bps).await
            }
        }
    }

    /// Execute a swap with simulation first
    async fn swap_with_simulation(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        aggregator: &Aggregator,
        route_config: &RouteConfig,
    ) -> Result<SwapResult> {
        // Simulate first to validate the swap
        match aggregator {
            Aggregator::Jupiter => {
                let jupiter = JupiterAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                let _simulate_result = jupiter
                    .simulate(input, output, amount, slippage_bps)
                    .await?;
            }
            Aggregator::Titan => {
                // Reuse existing Titan aggregator to avoid opening new WebSocket connections
                let titan = self.get_titan_aggregator().await?;
                let _simulate_result = titan.simulate(input, output, amount, slippage_bps).await?;
            }
        }

        // Then execute the swap
        self.swap_direct(
            input,
            output,
            amount,
            slippage_bps,
            aggregator,
            route_config,
        )
        .await
    }

    /// Execute swap using lowest slippage climber strategy
    async fn swap_lowest_slippage_climber(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        floor_slippage_bps: u16,
        max_slippage_bps: u16,
        step_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<SwapResult> {
        // Try each slippage level starting from floor
        let mut current_slippage = floor_slippage_bps;
        let mut last_error = None;

        while current_slippage <= max_slippage_bps {
            // Try swap with current slippage using best price strategy
            match self
                .swap_best_price(input, output, amount, current_slippage, route_config)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    last_error = Some(e);
                    current_slippage += step_bps;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("Failed to execute swap with any slippage level")))
    }

    /// Execute swap using best price (compare available aggregators)
    ///
    /// This is the default strategy when no routing strategy is specified.
    /// It compares output amounts from all available aggregators and selects the one
    /// that gives you the most tokens (best price).
    ///
    /// **This is what most users want** - maximizing the tokens you receive.
    async fn swap_best_price(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<SwapResult> {
        // Simulate both aggregators to compare output amounts
        let mut results = Vec::new();

        // Jupiter simulation
        if let Ok(jupiter) = JupiterAggregator::new_with_compute_price(
            &self.config,
            Arc::clone(&self.signer),
            route_config.compute_unit_price_micro_lamports,
        ) {
            if let Ok(sim_result) = jupiter.simulate(input, output, amount, slippage_bps).await {
                results.push((Aggregator::Jupiter, sim_result.out_amount));
            }
        }

        // Titan simulation (if configured)
        if self.config.is_titan_configured() {
            if let Ok(titan) = self.get_titan_aggregator().await {
                if let Ok(sim_result) = titan.simulate(input, output, amount, slippage_bps).await {
                    results.push((Aggregator::Titan, sim_result.out_amount));
                }
            }
        }

        if results.is_empty() {
            return Err(anyhow!("No aggregators available for swap"));
        }

        // Find aggregator with highest output amount (most tokens = best price)
        let (best_aggregator, _) = results
            .iter()
            .max_by_key(|(_, out_amount)| out_amount)
            .ok_or_else(|| anyhow!("Failed to determine best aggregator"))?;

        // Execute swap with aggregator that gives the most tokens
        self.swap_direct(
            input,
            output,
            amount,
            slippage_bps,
            best_aggregator,
            route_config,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_client_creation() {
        // This test would require a valid keypair, so we'll skip it for now
        // In a real scenario, you'd use a test keypair
    }
}

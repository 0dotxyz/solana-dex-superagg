use crate::aggregators::{
    dflow::DflowAggregator, jupiter::JupiterAggregator, titan::TitanAggregator, DexAggregator,
    SwapSummary, SwapTransaction,
};
use crate::config::{Aggregator, ClientConfig, OutputAtaBehavior, RouteConfig, RoutingStrategy};
use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing;

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

    /// Create an Associated Token Account (ATA) for the given mint if it doesn't exist
    ///
    /// # Arguments
    /// * `mint` - Token mint address (as string)
    ///
    /// # Returns
    /// `Ok(())` if the ATA exists or was successfully created, `Err` otherwise
    async fn create_ata_if_needed(&self, mint: &str) -> Result<()> {
        let mint_pubkey =
            Pubkey::from_str(mint).map_err(|e| anyhow!("Invalid mint address: {}", e))?;

        let owner_pubkey = self.signer.pubkey();

        // Get the mint account to determine the token program ID
        let mint_account = self
            .rpc_client
            .get_account(&mint_pubkey)
            .map_err(|e| anyhow!("Failed to get mint account: {}", e))?;

        let token_program_id = mint_account.owner;

        // Get the ATA address
        let ata_address = get_associated_token_address_with_program_id(
            &owner_pubkey,
            &mint_pubkey,
            &token_program_id,
        );

        // Check if the ATA already exists
        match self.rpc_client.get_account(&ata_address) {
            Ok(_) => {
                // ATA already exists, nothing to do
                return Ok(());
            }
            Err(_) => {
                // ATA doesn't exist, need to create it
            }
        }

        // Create the ATA instruction (idempotent - safe to call even if it exists)
        let create_ata_ix = create_associated_token_account_idempotent(
            &owner_pubkey,     // payer
            &owner_pubkey,     // owner
            &mint_pubkey,      // mint
            &token_program_id, // token program
        );

        // Build and send the transaction
        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| anyhow!("Failed to get recent blockhash: {}", e))?;

        let mut transaction = Transaction::new_with_payer(&[create_ata_ix], Some(&owner_pubkey));
        transaction.sign(&[self.signer.as_ref()], recent_blockhash);

        // Send and confirm the transaction
        let sig = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map_err(|e| anyhow!("Failed to create ATA: {}", e))?;

        // Wait a bit to ensure the ATA is fully available before proceeding
        // This helps avoid race conditions where the swap transaction is sent
        // before the ATA creation is fully processed
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Verify the ATA was actually created
        self.rpc_client.get_account(&ata_address).map_err(|e| {
            anyhow!(
                "ATA creation confirmed but account not found: {} (tx: {})",
                e,
                sig
            )
        })?;

        Ok(())
    }

    /// Execute a swap using default routing configuration
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    /// * `wrap_and_unwrap_sol` - Whether to wrap and unwrap SOL in the swap
    ///
    /// # Returns
    /// `SwapSummary` containing the swap result and all quote results
    ///
    /// Uses the default routing strategy from the client configuration.
    pub async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapSummary> {
        let mut route_config = self.config.default_route_config();
        route_config.wrap_and_unwrap_sol = wrap_and_unwrap_sol;
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
    /// `SwapSummary` containing the swap result and all quote results
    ///
    /// The route configuration allows per-swap customization of:
    /// - Compute unit price
    /// - Routing strategy (which aggregator to use)
    /// - Retry behavior
    /// - Output ATA creation behavior
    pub async fn swap_with_route_config(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: RouteConfig,
    ) -> Result<SwapSummary> {
        // Use slippage from route config if provided, otherwise use config default
        let slippage_bps = route_config
            .slippage_bps
            .unwrap_or(self.config.shared.slippage_bps);

        // Handle output ATA creation based on route config
        match route_config.output_ata {
            OutputAtaBehavior::Create => {
                // Create the output ATA if it doesn't exist
                self.create_ata_if_needed(output).await?;
            }
            OutputAtaBehavior::Ignore => {
                // Let the aggregator handle ATA creation
                // Nothing to do here
            }
        }

        // Determine which aggregator(s) to use based on routing strategy
        match &route_config.routing_strategy {
            Some(RoutingStrategy::BestPrice) | None => {
                // Compare available aggregators and use the one with best price (most tokens)
                self.swap_best_price(input, output, amount, slippage_bps, &route_config)
                    .await
            }
            Some(RoutingStrategy::PreferredAggregator { aggregator }) => {
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

    /// Build a signed exact-in swap transaction using the default route configuration, without
    /// submitting it.
    pub async fn build_swap_transaction(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapTransaction> {
        let mut route_config = self.config.default_route_config();
        route_config.wrap_and_unwrap_sol = wrap_and_unwrap_sol;
        self.build_swap_transaction_with_route_config(input, output, amount, route_config)
            .await
    }

    /// Build a signed exact-in swap transaction with a custom route configuration, without
    /// submitting it.
    pub async fn build_swap_transaction_with_route_config(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: RouteConfig,
    ) -> Result<SwapTransaction> {
        let slippage_bps = route_config
            .slippage_bps
            .unwrap_or(self.config.shared.slippage_bps);

        match &route_config.routing_strategy {
            Some(RoutingStrategy::BestPrice) | None => {
                self.build_swap_transaction_best_price(
                    input,
                    output,
                    amount,
                    slippage_bps,
                    &route_config,
                )
                .await
            }
            Some(RoutingStrategy::PreferredAggregator { aggregator }) => {
                self.build_swap_transaction_direct(
                    input,
                    output,
                    amount,
                    slippage_bps,
                    aggregator,
                    &route_config,
                )
                .await
            }
            Some(RoutingStrategy::LowestSlippageClimber {
                floor_slippage_bps,
                max_slippage_bps,
                step_bps,
            }) => {
                self.build_swap_transaction_lowest_slippage_climber(
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

    /// Build a signed exact-in swap transaction that is expected to yield at least `min_out`.
    ///
    /// The caller provides an initial input amount estimate. If the first route under-delivers,
    /// the input is bumped once by the shortfall ratio plus a small margin and re-routed.
    pub async fn build_swap_transaction_for_min_out(
        &self,
        input: &str,
        output: &str,
        initial_amount: u64,
        min_out: u64,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapTransaction> {
        let mut route_config = self.config.default_route_config();
        route_config.wrap_and_unwrap_sol = wrap_and_unwrap_sol;
        self.build_swap_transaction_for_min_out_with_route_config(
            input,
            output,
            initial_amount,
            min_out,
            route_config,
        )
        .await
    }

    /// Build a signed exact-in swap transaction that is expected to yield at least `min_out`, using
    /// a custom route configuration.
    pub async fn build_swap_transaction_for_min_out_with_route_config(
        &self,
        input: &str,
        output: &str,
        initial_amount: u64,
        min_out: u64,
        route_config: RouteConfig,
    ) -> Result<SwapTransaction> {
        if initial_amount == 0 {
            return Err(anyhow!(
                "initial input amount cannot be zero when building min-out swap"
            ));
        }

        let mut input_amount = initial_amount;
        let mut quoted_out = self
            .quote_out_amount_with_route_config(input, output, input_amount, &route_config)
            .await?;

        if quoted_out < min_out {
            let ratio = (min_out as f64 / quoted_out.max(1) as f64) * 1.02;
            input_amount = ((input_amount as f64) * ratio).ceil() as u64;
            quoted_out = self
                .quote_out_amount_with_route_config(input, output, input_amount, &route_config)
                .await?;

            if quoted_out < min_out {
                return Err(anyhow!(
                    "ExactIn quote still short after re-route: out {} < needed {}",
                    quoted_out,
                    min_out,
                ));
            }
        }

        let prepared = self
            .build_swap_transaction_with_route_config(input, output, input_amount, route_config)
            .await?;
        if prepared.out_amount < min_out {
            return Err(anyhow!(
                "ExactIn transaction quote short after build: out {} < needed {}",
                prepared.out_amount,
                min_out,
            ));
        }

        Ok(prepared)
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
    ) -> Result<SwapSummary> {
        let mut summary = match aggregator {
            Aggregator::Jupiter => {
                let jupiter = JupiterAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                jupiter
                    .swap(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        route_config.commitment_level,
                        route_config.wrap_and_unwrap_sol,
                    )
                    .await?
            }
            Aggregator::Titan => {
                // Reuse existing Titan aggregator to avoid opening new WebSocket connections
                let titan = self.get_titan_aggregator().await?;
                titan
                    .swap(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        route_config.commitment_level,
                        route_config.wrap_and_unwrap_sol,
                    )
                    .await?
            }
            Aggregator::Dflow => {
                let dflow = DflowAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                dflow
                    .swap(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        route_config.commitment_level,
                        route_config.wrap_and_unwrap_sol,
                    )
                    .await?
            }
        };

        // Ensure aggregator_used is set (should already be set by aggregators, but ensure it)
        if summary.swap_result.aggregator_used.is_none() {
            summary.swap_result.aggregator_used = Some(*aggregator);
        }

        Ok(summary)
    }

    /// Build a signed transaction directly with a specific aggregator.
    async fn build_swap_transaction_direct(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        aggregator: &Aggregator,
        route_config: &RouteConfig,
    ) -> Result<SwapTransaction> {
        let mut prepared = match aggregator {
            Aggregator::Jupiter => {
                let jupiter = JupiterAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                jupiter
                    .build_swap_transaction(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        route_config.wrap_and_unwrap_sol,
                    )
                    .await?
            }
            Aggregator::Titan => {
                let titan = self.get_titan_aggregator().await?;
                titan
                    .build_swap_transaction(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        route_config.wrap_and_unwrap_sol,
                    )
                    .await?
            }
            Aggregator::Dflow => {
                let dflow = DflowAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                dflow
                    .build_swap_transaction(
                        input,
                        output,
                        amount,
                        slippage_bps,
                        route_config.wrap_and_unwrap_sol,
                    )
                    .await?
            }
        };

        if prepared.aggregator_used.is_none() {
            prepared.aggregator_used = Some(*aggregator);
        }

        Ok(prepared)
    }

    /// Quote an exact-in swap directly with a specific aggregator and return only its output.
    async fn quote_out_amount_direct(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        aggregator: &Aggregator,
        route_config: &RouteConfig,
    ) -> Result<u64> {
        let quote_result = match aggregator {
            Aggregator::Jupiter => {
                let jupiter = JupiterAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                jupiter.quote(input, output, amount, slippage_bps).await?
            }
            Aggregator::Titan => {
                let titan = self.get_titan_aggregator().await?;
                titan.quote(input, output, amount, slippage_bps).await?
            }
            Aggregator::Dflow => {
                let dflow = DflowAggregator::new_with_compute_price(
                    &self.config,
                    Arc::clone(&self.signer),
                    route_config.compute_unit_price_micro_lamports,
                )?;
                dflow.quote(input, output, amount, slippage_bps).await?
            }
        };

        Ok(quote_result.out_amount)
    }

    /// Quote an exact-in swap using the provided route configuration and return only its output.
    async fn quote_out_amount_with_route_config(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: &RouteConfig,
    ) -> Result<u64> {
        let slippage_bps = route_config
            .slippage_bps
            .unwrap_or(self.config.shared.slippage_bps);

        match &route_config.routing_strategy {
            Some(RoutingStrategy::BestPrice) | None => {
                self.quote_out_amount_best_price(input, output, amount, slippage_bps, route_config)
                    .await
            }
            Some(RoutingStrategy::PreferredAggregator { aggregator }) => {
                self.quote_out_amount_direct(
                    input,
                    output,
                    amount,
                    slippage_bps,
                    aggregator,
                    route_config,
                )
                .await
            }
            Some(RoutingStrategy::LowestSlippageClimber {
                floor_slippage_bps,
                max_slippage_bps,
                step_bps,
            }) => {
                let mut current_slippage = *floor_slippage_bps;
                let mut last_error = None;
                while current_slippage <= *max_slippage_bps {
                    match self
                        .quote_out_amount_best_price(
                            input,
                            output,
                            amount,
                            current_slippage,
                            route_config,
                        )
                        .await
                    {
                        Ok(out_amount) => return Ok(out_amount),
                        Err(e) => {
                            last_error = Some(e);
                            current_slippage += *step_bps;
                        }
                    }
                }
                Err(last_error
                    .unwrap_or_else(|| anyhow!("Failed to quote swap with any slippage level")))
            }
        }
    }

    /// Build a signed transaction using the lowest slippage climber strategy.
    async fn build_swap_transaction_lowest_slippage_climber(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        floor_slippage_bps: u16,
        max_slippage_bps: u16,
        step_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<SwapTransaction> {
        let mut current_slippage = floor_slippage_bps;
        let mut last_error = None;
        let mut attempt_count = 0;

        while current_slippage <= max_slippage_bps {
            attempt_count += 1;
            match self
                .build_swap_transaction_best_price(
                    input,
                    output,
                    amount,
                    current_slippage,
                    route_config,
                )
                .await
            {
                Ok(mut prepared) => {
                    if prepared.slippage_bps_used.is_none() {
                        prepared.slippage_bps_used = Some(current_slippage);
                    }
                    return Ok(prepared);
                }
                Err(e) => {
                    last_error = Some(e);
                    current_slippage += step_bps;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "Failed to build swap transaction with any slippage level (tried {} attempts)",
                attempt_count
            )
        }))
    }

    /// Execute swap using lowest slippage climber strategy
    ///
    /// This strategy tries multiple slippage levels starting from `floor_slippage_bps`
    /// and incrementing by `step_bps` until a swap succeeds or `max_slippage_bps` is reached.
    /// The returned `SwapSummary` includes `slippage_bps_used` to show which slippage level succeeded.
    ///
    /// This function will log warnings if slippage climbs higher than the floor, indicating
    /// that the swap required more slippage tolerance than initially expected.
    async fn swap_lowest_slippage_climber(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        floor_slippage_bps: u16,
        max_slippage_bps: u16,
        step_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<SwapSummary> {
        // Try each slippage level starting from floor
        let mut current_slippage = floor_slippage_bps;
        let mut last_error = None;
        let mut attempt_count = 0;

        tracing::debug!(
            floor_slippage_bps = floor_slippage_bps,
            max_slippage_bps = max_slippage_bps,
            step_bps = step_bps,
            "Climbing slippage staircase"
        );

        while current_slippage <= max_slippage_bps {
            attempt_count += 1;
            tracing::debug!(
                attempt = attempt_count,
                slippage_bps = current_slippage,
                slippage_percent = current_slippage as f64 / 100.0,
                "Attempting swap with slippage"
            );

            // Try swap with current slippage using best price strategy
            match self
                .swap_best_price(input, output, amount, current_slippage, route_config)
                .await
            {
                Ok(mut summary) => {
                    // Ensure slippage_bps_used is set (should already be set by aggregators, but ensure it)
                    if summary.swap_result.slippage_bps_used.is_none() {
                        summary.swap_result.slippage_bps_used = Some(current_slippage);
                    }

                    let slippage_used = summary
                        .swap_result
                        .slippage_bps_used
                        .unwrap_or(current_slippage);

                    // Show aggregator used
                    if let Some(agg) = &summary.swap_result.aggregator_used {
                        let agg_name = match agg {
                            Aggregator::Jupiter => "Jupiter",
                            Aggregator::Titan => "Titan",
                            Aggregator::Dflow => "DFlow",
                        };
                        tracing::debug!(aggregator = agg_name, "Aggregator used");
                    }

                    // Warn if slippage climbed higher than floor
                    if slippage_used > floor_slippage_bps {
                        let excess_slippage = slippage_used - floor_slippage_bps;
                        let excess_percent =
                            (excess_slippage as f64 / floor_slippage_bps as f64) * 100.0;
                        tracing::warn!(
                            floor_slippage_bps = floor_slippage_bps,
                            floor_slippage_percent = floor_slippage_bps as f64 / 100.0,
                            actual_slippage_bps = slippage_used,
                            actual_slippage_percent = slippage_used as f64 / 100.0,
                            excess_slippage_bps = excess_slippage,
                            excess_percent = excess_percent,
                            attempts = attempt_count,
                            "Slippage climbed higher than expected"
                        );
                    } else {
                        tracing::info!(
                            slippage_bps = slippage_used,
                            "Swap succeeded at floor slippage"
                        );
                    }

                    return Ok(summary);
                }
                Err(e) => {
                    tracing::debug!(
                        slippage_bps = current_slippage,
                        error = %e,
                        "Swap failed at slippage level"
                    );
                    last_error = Some(e);
                    current_slippage += step_bps;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "Failed to execute swap with any slippage level (tried {} attempts)",
                attempt_count
            )
        }))
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
    ) -> Result<SwapSummary> {
        // Quote available aggregators to compare output amounts
        let mut quote_results = Vec::new();
        let mut comparison_results = Vec::new();

        // Jupiter quote
        if let Ok(jupiter) = JupiterAggregator::new_with_compute_price(
            &self.config,
            Arc::clone(&self.signer),
            route_config.compute_unit_price_micro_lamports,
        ) {
            if let Ok(quote_result) = jupiter.quote(input, output, amount, slippage_bps).await {
                quote_results.push((Aggregator::Jupiter, quote_result.clone()));
                comparison_results.push((Aggregator::Jupiter, quote_result.out_amount));
            }
        }

        // Titan quote (if configured)
        if self.config.is_titan_configured() {
            if let Ok(titan) = self.get_titan_aggregator().await {
                if let Ok(quote_result) = titan.quote(input, output, amount, slippage_bps).await {
                    quote_results.push((Aggregator::Titan, quote_result.clone()));
                    comparison_results.push((Aggregator::Titan, quote_result.out_amount));
                }
            }
        }

        // DFlow quote (if configured)
        if self.config.is_dflow_configured() {
            if let Ok(dflow) = DflowAggregator::new_with_compute_price(
                &self.config,
                Arc::clone(&self.signer),
                route_config.compute_unit_price_micro_lamports,
            ) {
                if let Ok(quote_result) = dflow.quote(input, output, amount, slippage_bps).await {
                    quote_results.push((Aggregator::Dflow, quote_result.clone()));
                    comparison_results.push((Aggregator::Dflow, quote_result.out_amount));
                }
            }
        }

        if comparison_results.is_empty() {
            return Err(anyhow!("No aggregators available for swap"));
        }

        // Find aggregator with highest output amount (most tokens = best price)
        let (best_aggregator, best_out_amount) = comparison_results
            .iter()
            .max_by_key(|(_, out_amount)| out_amount)
            .ok_or_else(|| anyhow!("Failed to determine best aggregator"))?;

        tracing::debug!("Comparing aggregators");
        for (agg, out_amt) in &comparison_results {
            let agg_name = match agg {
                Aggregator::Jupiter => "Jupiter",
                Aggregator::Titan => "Titan",
                Aggregator::Dflow => "DFlow",
            };
            tracing::debug!(
                aggregator = agg_name,
                output_amount = out_amt,
                "Aggregator quote"
            );
        }
        let best_name = match best_aggregator {
            Aggregator::Jupiter => "Jupiter",
            Aggregator::Titan => "Titan",
            Aggregator::Dflow => "DFlow",
        };
        tracing::info!(
            aggregator = best_name,
            output_amount = best_out_amount,
            "Selected aggregator with best price"
        );

        // Execute swap with aggregator that gives the most tokens
        let executed_summary = self
            .swap_direct(
                input,
                output,
                amount,
                slippage_bps,
                best_aggregator,
                route_config,
            )
            .await?;

        Ok(SwapSummary {
            swap_result: executed_summary.swap_result,
            quote_results,
        })
    }

    /// Build a signed transaction using best price routing.
    async fn build_swap_transaction_best_price(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<SwapTransaction> {
        let mut comparison_results = Vec::new();

        if let Ok(jupiter) = JupiterAggregator::new_with_compute_price(
            &self.config,
            Arc::clone(&self.signer),
            route_config.compute_unit_price_micro_lamports,
        ) {
            if let Ok(quote_result) = jupiter.quote(input, output, amount, slippage_bps).await {
                comparison_results.push((Aggregator::Jupiter, quote_result.out_amount));
            }
        }

        if self.config.is_titan_configured() {
            if let Ok(titan) = self.get_titan_aggregator().await {
                if let Ok(quote_result) = titan.quote(input, output, amount, slippage_bps).await {
                    comparison_results.push((Aggregator::Titan, quote_result.out_amount));
                }
            }
        }

        if self.config.is_dflow_configured() {
            if let Ok(dflow) = DflowAggregator::new_with_compute_price(
                &self.config,
                Arc::clone(&self.signer),
                route_config.compute_unit_price_micro_lamports,
            ) {
                if let Ok(quote_result) = dflow.quote(input, output, amount, slippage_bps).await {
                    comparison_results.push((Aggregator::Dflow, quote_result.out_amount));
                }
            }
        }

        if comparison_results.is_empty() {
            return Err(anyhow!("No aggregators available for swap transaction"));
        }

        let (best_aggregator, best_out_amount) = comparison_results
            .iter()
            .max_by_key(|(_, out_amount)| out_amount)
            .ok_or_else(|| anyhow!("Failed to determine best aggregator"))?;

        let best_name = match best_aggregator {
            Aggregator::Jupiter => "Jupiter",
            Aggregator::Titan => "Titan",
            Aggregator::Dflow => "DFlow",
        };
        tracing::info!(
            aggregator = best_name,
            output_amount = best_out_amount,
            "Selected aggregator for prepared swap transaction"
        );

        self.build_swap_transaction_direct(
            input,
            output,
            amount,
            slippage_bps,
            best_aggregator,
            route_config,
        )
        .await
    }

    /// Quote an exact-in swap using best price routing and return only its output amount.
    async fn quote_out_amount_best_price(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<u64> {
        let mut comparison_results = Vec::new();

        if let Ok(jupiter) = JupiterAggregator::new_with_compute_price(
            &self.config,
            Arc::clone(&self.signer),
            route_config.compute_unit_price_micro_lamports,
        ) {
            if let Ok(quote_result) = jupiter.quote(input, output, amount, slippage_bps).await {
                comparison_results.push((Aggregator::Jupiter, quote_result.out_amount));
            }
        }

        if self.config.is_titan_configured() {
            if let Ok(titan) = self.get_titan_aggregator().await {
                if let Ok(quote_result) = titan.quote(input, output, amount, slippage_bps).await {
                    comparison_results.push((Aggregator::Titan, quote_result.out_amount));
                }
            }
        }

        if self.config.is_dflow_configured() {
            if let Ok(dflow) = DflowAggregator::new_with_compute_price(
                &self.config,
                Arc::clone(&self.signer),
                route_config.compute_unit_price_micro_lamports,
            ) {
                if let Ok(quote_result) = dflow.quote(input, output, amount, slippage_bps).await {
                    comparison_results.push((Aggregator::Dflow, quote_result.out_amount));
                }
            }
        }

        comparison_results
            .iter()
            .max_by_key(|(_, out_amount)| out_amount)
            .map(|(_, out_amount)| *out_amount)
            .ok_or_else(|| anyhow!("No aggregators available for swap quote"))
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

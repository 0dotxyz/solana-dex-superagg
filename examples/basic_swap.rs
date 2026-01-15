//! Basic swap examples demonstrating all routing strategies and features
//!
//! This example shows how to use the solana-dex-superagg library with different
//! routing strategies and commitment levels.
//!
//! # Setup
//!
//! Set the following environment variables before running:
//!
//! ```bash
//! export DEX_SUPERAGG_SHARED__RPC_URL="https://api.mainnet-beta.solana.com"
//! export DEX_SUPERAGG_SHARED__WALLET_KEYPAIR="your_keypair_here"
//! export DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL="https://api.jup.ag/swap/v1"
//! export DEX_SUPERAGG_JUPITER__API_KEY="your_jupiter_api_key"
//! export DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT="us1.api.demo.titan.exchange"
//! export DEX_SUPERAGG_TITAN__TITAN_API_KEY="your_titan_api_key"
//! ```
//!
//! # Running
//!
//! ```bash
//! cargo run --example basic_swap
//! ```

use anyhow::Result;
use solana_dex_superagg::{
    client::DexSuperAggClient,
    config::{Aggregator, ClientConfig, RouteConfig, RoutingStrategy},
};
use solana_sdk::commitment_config::CommitmentLevel;
use tracing;

/// Token addresses
const SOL_TOKEN: &str = "So11111111111111111111111111111111111111112";
const USDC_TOKEN: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing with INFO level by default if RUST_LOG is not set
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    tracing::info!("Starting Solana DEX SuperAgg Examples");

    // Load .env file if it exists (ignore errors if it doesn't exist)
    let _ = dotenvy::dotenv();

    // Load config from environment variables
    let config = ClientConfig::from_env()?;

    // Validate config (optional but recommended)
    tracing::info!("Validating configuration");
    if let Err(errors) = config.validate().await {
        for error in errors {
            tracing::error!(error = %error, "Configuration error");
        }
        return Err(anyhow::anyhow!("Invalid configuration"));
    }
    tracing::info!("Configuration valid");

    // Create client
    let client = DexSuperAggClient::new(config)?;
    tracing::info!("Client created");

    // Token addresses
    let sol = SOL_TOKEN;
    let usdc = USDC_TOKEN;

    // Use a very small amount for testing (approximately $0.01 USD worth of SOL)
    // Assuming 1 SOL = $100, so 0.01 USD = 0.0001 SOL = 100,000 lamports
    // This matches the amount used in integration tests
    let amount = 100_000; // Very small amount for testing (~$0.01 USD)

    // Example 1: Simple swap (uses default strategy from config - usually BestPrice)
    // This is the easiest way to swap - no configuration needed!
    tracing::info!("Example 1: Simple Swap (Default Strategy)");
    let summary = client.swap(sol, usdc, amount).await?;
    tracing::info!(
        transaction = %summary.swap_result.signature,
        output_amount = summary.swap_result.out_amount,
        aggregator = ?summary.swap_result.aggregator_used,
        execution_time_ms = summary.swap_result.execution_time.map(|t| t.as_secs_f64() * 1000.0),
        "Swap successful"
    );

    // Example 2: Best Price Strategy (explicitly compare all aggregators)
    // This compares Jupiter and Titan (if configured) and uses whichever gives more tokens
    tracing::info!("Example 2: Best Price Strategy");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        slippage_bps: Some(25), // Override default slippage to 0.25%
        commitment_level: CommitmentLevel::Confirmed, // Use Confirmed for faster execution
        ..Default::default()
    };
    let summary = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    tracing::info!(
        transaction = %summary.swap_result.signature,
        output_amount = summary.swap_result.out_amount,
        aggregator = ?summary.swap_result.aggregator_used,
        slippage_bps = summary.swap_result.slippage_bps_used,
        slippage_percent = summary.swap_result.slippage_bps_used.map(|s| s as f64 / 100.0),
        "Swap successful"
    );
    // Show simulation results for all aggregators that were tried
    for (agg, sim_result) in &summary.sim_results {
        let agg_name = match agg {
            Aggregator::Jupiter => "Jupiter",
            Aggregator::Titan => "Titan",
            Aggregator::Dflow => "DFlow",
        };
        tracing::debug!(
            aggregator = agg_name,
            output_amount = sim_result.out_amount,
            sim_time_ms = sim_result.sim_time.map(|t| t.as_secs_f64() * 1000.0),
            "Simulation result"
        );
    }

    // Example 3: Preferred Aggregator - Use Jupiter
    // Force the swap to use Jupiter instead of comparing aggregators
    tracing::info!("Example 3: Preferred Aggregator (Jupiter)");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Jupiter,
            simulate: false, // Set to true to simulate before executing
        }),
        slippage_bps: Some(25),
        commitment_level: CommitmentLevel::Confirmed, // Fast confirmation
        ..Default::default()
    };
    let summary = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    tracing::info!(
        transaction = %summary.swap_result.signature,
        output_amount = summary.swap_result.out_amount,
        aggregator = ?summary.swap_result.aggregator_used,
        "Swap successful"
    );

    // Example 4: Preferred Aggregator - Use Titan
    if client.config().is_titan_configured() {
        tracing::info!("Example 4: Preferred Aggregator (Titan)");
        let route_config = RouteConfig {
            routing_strategy: Some(RoutingStrategy::PreferredAggregator {
                aggregator: Aggregator::Titan,
                simulate: false,
            }),
            slippage_bps: Some(25),
            commitment_level: CommitmentLevel::Confirmed,
            ..Default::default()
        };
        let summary = client
            .swap_with_route_config(sol, usdc, amount, route_config)
            .await?;
        tracing::info!(
            transaction = %summary.swap_result.signature,
            output_amount = summary.swap_result.out_amount,
            aggregator = ?summary.swap_result.aggregator_used,
            "Swap successful"
        );
    } else {
        tracing::warn!("Example 4: Preferred Aggregator (Titan) - Skipped: Titan not configured");
    }

    // Example 4.5: Preferred Aggregator - Use DFlow
    if client.config().is_dflow_configured() {
        tracing::info!("Example 4.5: Preferred Aggregator (DFlow)");
        let route_config = RouteConfig {
            routing_strategy: Some(RoutingStrategy::PreferredAggregator {
                aggregator: Aggregator::Dflow,
                simulate: false,
            }),
            slippage_bps: Some(25),
            commitment_level: CommitmentLevel::Confirmed,
            ..Default::default()
        };
        let summary = client
            .swap_with_route_config(sol, usdc, amount, route_config)
            .await?;
        tracing::info!(
            transaction = %summary.swap_result.signature,
            output_amount = summary.swap_result.out_amount,
            aggregator = ?summary.swap_result.aggregator_used,
            "Swap successful"
        );
    } else {
        tracing::warn!("Example 4.5: Preferred Aggregator (DFlow) - Skipped: DFlow not configured");
    }

    // Example 5: Lowest Slippage Climber (Staircase Strategy)
    // Tests multiple slippage levels starting from floor_slippage_bps and incrementing by step_bps
    // until a swap succeeds or max_slippage_bps is reached
    // This is useful for finding the minimum slippage needed for a swap
    tracing::info!("Example 5: Lowest Slippage Climber (Staircase)");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::LowestSlippageClimber {
            floor_slippage_bps: 10, // Start at 0.1%
            max_slippage_bps: 100,  // Up to 1%
            step_bps: 10,           // Step by 0.1%
        }),
        commitment_level: CommitmentLevel::Confirmed,
        ..Default::default()
    };
    let summary = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    tracing::info!(
        transaction = %summary.swap_result.signature,
        output_amount = summary.swap_result.out_amount,
        aggregator = ?summary.swap_result.aggregator_used,
        slippage_bps = summary.swap_result.slippage_bps_used,
        slippage_percent = summary.swap_result.slippage_bps_used.map(|s| s as f64 / 100.0),
        "Swap successful"
    );

    // Example 6: Use Finalized commitment for critical swaps
    // Finalized provides absolute guarantee but takes ~15 seconds
    tracing::info!("Example 6: Critical Swap with Finalized Commitment");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        commitment_level: CommitmentLevel::Finalized, // Use Finalized for critical swaps
        ..Default::default()
    };
    let summary = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    tracing::info!(
        transaction = %summary.swap_result.signature,
        execution_time_ms = summary.swap_result.execution_time.map(|t| t.as_secs_f64() * 1000.0),
        "Swap finalized"
    );

    // Clean up when done (closes Titan WebSocket connection if used)
    client.close().await?;

    tracing::info!("All examples completed successfully");
    Ok(())
}

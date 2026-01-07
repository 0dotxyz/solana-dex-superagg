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
use std::time::Duration;

/// Token addresses
const SOL_TOKEN: &str = "So11111111111111111111111111111111111111112";
const USDC_TOKEN: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

fn format_duration_ms(d: Option<Duration>) -> String {
    match d {
        Some(dur) => format!("{:.2} ms", dur.as_secs_f64() * 1000.0),
        None => "N/A".to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Solana DEX SuperAgg Examples ===\n");

    // Load config from environment variables
    let config = ClientConfig::from_env()?;

    // Validate config (optional but recommended)
    println!("Validating configuration...");
    if let Err(errors) = config.validate().await {
        eprintln!("Configuration errors:");
        for error in errors {
            eprintln!("  - {}", error);
        }
        return Err(anyhow::anyhow!("Invalid configuration"));
    }
    println!("✓ Configuration valid\n");

    // Create client
    let client = DexSuperAggClient::new(config)?;
    println!("✓ Client created\n");

    // Token addresses
    let sol = SOL_TOKEN;
    let usdc = USDC_TOKEN;

    // Use a very small amount for testing (approximately $0.01 USD worth of SOL)
    // Assuming 1 SOL = $100, so 0.01 USD = 0.0001 SOL = 100,000 lamports
    // This matches the amount used in integration tests
    let amount = 100_000; // Very small amount for testing (~$0.01 USD)

    // Example 1: Simple swap (uses default strategy from config - usually BestPrice)
    // This is the easiest way to swap - no configuration needed!
    println!("=== Example 1: Simple Swap (Default Strategy) ===");
    let summary = client.swap(sol, usdc, amount).await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", summary.swap_result.signature);
    println!("  Output: {} lamports", summary.swap_result.out_amount);
    if let Some(agg) = summary.swap_result.aggregator_used {
        println!("  Aggregator: {:?}", agg);
    }
    if let Some(time) = summary.swap_result.execution_time {
        println!("  Execution time: {}", format_duration_ms(Some(time)));
    }
    println!();

    // Example 2: Best Price Strategy (explicitly compare all aggregators)
    // This compares Jupiter and Titan (if configured) and uses whichever gives more tokens
    println!("=== Example 2: Best Price Strategy ===");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        slippage_bps: Some(25), // Override default slippage to 0.25%
        commitment_level: CommitmentLevel::Confirmed, // Use Confirmed for faster execution
        ..Default::default()
    };
    let summary = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", summary.swap_result.signature);
    println!("  Output: {} lamports", summary.swap_result.out_amount);
    if let Some(agg) = summary.swap_result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    if let Some(slippage) = summary.swap_result.slippage_bps_used {
        println!(
            "  Slippage used: {} bps ({:.2}%)",
            slippage,
            slippage as f64 / 100.0
        );
    }
    // Show simulation results for all aggregators that were tried
    for (agg, sim_result) in &summary.sim_results {
        println!(
            "  {} simulation: {} lamports (sim time: {})",
            match agg {
                Aggregator::Jupiter => "Jupiter",
                Aggregator::Titan => "Titan",
            },
            sim_result.out_amount,
            format_duration_ms(sim_result.sim_time)
        );
    }
    println!();

    // Example 3: Preferred Aggregator - Use Jupiter
    // Force the swap to use Jupiter instead of comparing aggregators
    println!("=== Example 3: Preferred Aggregator (Jupiter) ===");
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
    println!("✓ Swap successful!");
    println!("  Transaction: {}", summary.swap_result.signature);
    println!("  Output: {} lamports", summary.swap_result.out_amount);
    if let Some(agg) = summary.swap_result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    println!();

    // Example 4: Preferred Aggregator - Use Titan
    if client.config().is_titan_configured() {
        println!("=== Example 4: Preferred Aggregator (Titan) ===");
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
        println!("✓ Swap successful!");
        println!("  Transaction: {}", summary.swap_result.signature);
        println!("  Output: {} lamports", summary.swap_result.out_amount);
        if let Some(agg) = summary.swap_result.aggregator_used {
            println!("  Aggregator used: {:?}", agg);
        }
        println!();
    } else {
        println!("=== Example 4: Preferred Aggregator (Titan) ===");
        println!("⚠ Skipped: Titan not configured\n");
    }

    // Example 5: Lowest Slippage Climber (Staircase Strategy)
    // Tests multiple slippage levels starting from floor_slippage_bps and incrementing by step_bps
    // until a swap succeeds or max_slippage_bps is reached
    // This is useful for finding the minimum slippage needed for a swap
    println!("=== Example 5: Lowest Slippage Climber (Staircase) ===");
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
    println!("✓ Swap successful!");
    println!("  Transaction: {}", summary.swap_result.signature);
    println!("  Output: {} lamports", summary.swap_result.out_amount);
    if let Some(agg) = summary.swap_result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    if let Some(slippage) = summary.swap_result.slippage_bps_used {
        println!(
            "  Slippage used: {} bps ({:.2}%)",
            slippage,
            slippage as f64 / 100.0
        );
        // The staircase strategy will warn if slippage climbed higher than the floor
    }
    println!();

    // Example 6: Use Finalized commitment for critical swaps
    // Finalized provides absolute guarantee but takes ~15 seconds
    println!("=== Example 6: Critical Swap with Finalized Commitment ===");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        commitment_level: CommitmentLevel::Finalized, // Use Finalized for critical swaps
        ..Default::default()
    };
    let summary = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    println!("✓ Swap finalized!");
    println!("  Transaction: {}", summary.swap_result.signature);
    if let Some(time) = summary.swap_result.execution_time {
        println!("  Execution time: {}", format_duration_ms(Some(time)));
    }
    println!();

    // Clean up when done (closes Titan WebSocket connection if used)
    client.close().await?;

    println!("=== All Examples Completed Successfully ===");
    Ok(())
}

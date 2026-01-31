//! Standalone test for DFlow aggregator
//!
//! This test verifies that DFlow swaps work correctly.
//!
//! Environment variables required:
//! - `DEX_SUPERAGG_SHARED__RPC_URL`: Solana RPC endpoint (required)
//! - `DEX_SUPERAGG_SHARED__WALLET_KEYPAIR`: Wallet keypair (base58, JSON array, or comma-separated bytes) (required)
//! - `DEX_SUPERAGG_DFLOW__API_URL`: DFlow API URL (required for DFlow tests)
//! - `DEX_SUPERAGG_DFLOW__API_KEY`: DFlow API key (optional)
//!
//! Optional environment variables:
//! - `DEX_SUPERAGG_SHARED__SLIPPAGE_BPS`: Slippage in basis points (default: 25)
//! - `TEST_AMOUNT_USD`: Amount in USD to swap (default: 0.01)
//!
//! Run with: `cargo test --test dflow_test -- --nocapture --ignored`

use anyhow::Result;
use solana_dex_superagg::{
    client::DexSuperAggClient,
    config::{Aggregator, ClientConfig, RouteConfig, RoutingStrategy},
};
use std::time::Duration;

/// Token addresses
const SOL_TOKEN: &str = "So11111111111111111111111111111111111111112";
const USDC_TOKEN: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Convert USD amount to SOL lamports (SOL has 9 decimals)
/// This is approximate - assumes 1 SOL = $100 for testing purposes
fn usd_to_sol_lamports(usd_amount: f64) -> u64 {
    // Approximate: 1 SOL = $100, so 0.01 USD = 0.0001 SOL = 100,000 lamports
    // Formula: (usd_amount / 100.0) * 1_000_000_000 lamports per SOL
    (usd_amount / 100.0 * 1_000_000_000.0) as u64
}

fn format_duration_ms(d: Option<Duration>) -> String {
    match d {
        Some(dur) => format!("{:.2} ms", dur.as_secs_f64() * 1000.0),
        None => "N/A".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore] // Ignore by default - requires environment setup
async fn test_dflow_swap() -> Result<()> {
    println!("=== DFlow Integration Test ===\n");

    // Load .env file if it exists (ignore errors if it doesn't exist)
    let _ = dotenvy::dotenv();

    // Load configuration from environment
    let config = ClientConfig::from_env()
        .map_err(|e| anyhow::anyhow!("Failed to load config from env: {}", e))?;

    // Validate configuration
    println!("Validating configuration...");
    if let Err(errors) = config.validate().await {
        return Err(anyhow::anyhow!(
            "Configuration validation failed:\n{}",
            errors.join("\n")
        ));
    }
    println!("✓ Configuration valid\n");

    // Check if DFlow is configured
    if !config.is_dflow_configured() {
        return Err(anyhow::anyhow!(
            "DFlow is not configured. Set DEX_SUPERAGG_DFLOW__API_URL to enable DFlow."
        ));
    }

    // Create client
    let client = DexSuperAggClient::new(config)?;
    println!("✓ Client created\n");

    // Get test amount from environment (default: 0.01 USD)
    let test_amount_usd: f64 = std::env::var("TEST_AMOUNT_USD")
        .unwrap_or_else(|_| "0.01".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid TEST_AMOUNT_USD: {}", e))?;

    let slippage_bps: u16 = std::env::var("DEX_SUPERAGG_SHARED__SLIPPAGE_BPS")
        .unwrap_or_else(|_| "25".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid SLIPPAGE_BPS: {}", e))?;

    println!("Test Configuration:");
    println!("  Amount: ${} USD", test_amount_usd);
    println!(
        "  Slippage: {} bps ({:.2}%)",
        slippage_bps,
        slippage_bps as f64 / 100.0
    );
    println!("  Input Token: SOL");
    println!("  Output Token: USDC");
    println!();

    // Convert to lamports
    let sol_amount = usd_to_sol_lamports(test_amount_usd);
    println!("  Input Amount: {} SOL lamports", sol_amount);
    println!();

    // Test: Simple DFlow Swap
    println!("=== Test: Simple DFlow Swap ===");
    println!(
        "Swapping {} lamports of {} -> {}",
        sol_amount, SOL_TOKEN, USDC_TOKEN
    );

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Dflow,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary = client
        .swap_with_route_config(SOL_TOKEN, USDC_TOKEN, sol_amount, route_config, true)
        .await?;

    println!("  ✓ Swap successful!");
    println!("  Transaction: {}", summary.swap_result.signature);
    println!(
        "  Output Amount: {} lamports",
        summary.swap_result.out_amount
    );
    if let Some(agg) = summary.swap_result.aggregator_used {
        let agg_name = match agg {
            Aggregator::Titan => "Titan",
            Aggregator::Jupiter => "Jupiter",
            Aggregator::Dflow => "DFlow",
        };
        println!("  Aggregator Used: {}", agg_name);
    }
    if let Some(slippage_used) = summary.swap_result.slippage_bps_used {
        println!(
            "  Slippage Used: {} bps ({:.2}%)",
            slippage_used,
            slippage_used as f64 / 100.0
        );
        // Validate slippage used is <= slippage requested
        assert!(
            slippage_used <= slippage_bps,
            "Slippage used ({}) should be <= slippage requested ({})",
            slippage_used,
            slippage_bps
        );
    }
    if let Some(exec_time) = summary.swap_result.execution_time {
        println!("  Execution Time: {}", format_duration_ms(Some(exec_time)));
    }

    // Show quote results if available
    if !summary.quote_results.is_empty() {
        println!("\n  Quote Results:");
        for (agg, sim_result) in &summary.quote_results {
            let agg_name = match agg {
                Aggregator::Titan => "Titan",
                Aggregator::Jupiter => "Jupiter",
                Aggregator::Dflow => "DFlow",
            };
            println!("    {}: {} lamports", agg_name, sim_result.out_amount);
            if let Some(quote_time) = sim_result.quote_time {
                println!("      Quote Time: {}", format_duration_ms(Some(quote_time)));
            }
        }
    }

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n=== Test: DFlow Swap Back ===");
    println!("Swapping back: {} -> {}", USDC_TOKEN, SOL_TOKEN);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Dflow,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary_back = client
        .swap_with_route_config(
            USDC_TOKEN,
            SOL_TOKEN,
            summary.swap_result.out_amount,
            route_config_back,
            true,
        )
        .await?;

    println!("  ✓ Swap back successful!");
    println!("  Transaction: {}", summary_back.swap_result.signature);
    println!(
        "  Output Amount: {} lamports",
        summary_back.swap_result.out_amount
    );
    if let Some(slippage_used) = summary_back.swap_result.slippage_bps_used {
        println!(
            "  Slippage Used: {} bps ({:.2}%)",
            slippage_used,
            slippage_used as f64 / 100.0
        );
        // Validate slippage used is <= slippage requested
        assert!(
            slippage_used <= slippage_bps,
            "Slippage used ({}) should be <= slippage requested ({})",
            slippage_used,
            slippage_bps
        );
    }
    if let Some(exec_time) = summary_back.swap_result.execution_time {
        println!("  Execution Time: {}", format_duration_ms(Some(exec_time)));
    }

    println!("\n=== All DFlow Tests Completed Successfully ===");
    Ok(())
}

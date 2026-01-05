//! Integration test for all swap methods
//!
//! This test verifies that all swap strategies work correctly:
//! - Simple Titan Swap
//! - Simple Jupiter Swap
//! - Best Price (compares both aggregators)
//! - Staircase (LowestSlippageClimber strategy)
//!
//! Environment variables required:
//! - `DEX_SUPERAGG_SHARED__RPC_URL`: Solana RPC endpoint (required)
//! - `DEX_SUPERAGG_SHARED__WALLET_KEYPAIR`: Wallet keypair (base58, JSON array, or comma-separated bytes) (required)
//! - `DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT`: Titan WebSocket endpoint (required for Titan tests)
//! - `DEX_SUPERAGG_TITAN__TITAN_API_KEY` or `DEX_SUPERAGG_TITAN__HERMES_ENDPOINT`: Titan auth (required for Titan tests)
//!
//! Optional environment variables:
//! - `DEX_SUPERAGG_SHARED__SLIPPAGE_BPS`: Slippage in basis points (default: 25)
//! - `DEX_SUPERAGG_JUPITER__JUPITER_API_KEY`: Jupiter API key (optional, recommended for production)
//!   Get your API key from https://portal.jup.ag/
//! - `TEST_AMOUNT_USD`: Amount in USD to swap (default: 0.01)
//!
//! Run with: `cargo test --test integration_swap_test -- --ignored --nocapture`

use anyhow::Result;
use solana_dex_superagg::{
    client::DexSuperAggClient, config::Aggregator, config::ClientConfig, config::RouteConfig,
    config::RoutingStrategy,
};

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

#[tokio::test(flavor = "multi_thread")]
#[ignore] // Ignore by default - requires environment setup
async fn test_all_swap_methods() -> Result<()> {
    println!("=== Integration Test: All Swap Methods ===\n");

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

    // Test 1: Simple Titan Swap
    if client.config().is_titan_configured() {
        println!("=== Test 1: Simple Titan Swap ===");
        test_titan_swap(&client, SOL_TOKEN, USDC_TOKEN, sol_amount, slippage_bps).await?;
        println!();
    } else {
        println!("=== Test 1: Simple Titan Swap ===");
        println!("⚠ Skipped: Titan not configured\n");
    }

    // Test 2: Simple Jupiter Swap
    println!("=== Test 2: Simple Jupiter Swap ===");
    test_jupiter_swap(&client, SOL_TOKEN, USDC_TOKEN, sol_amount, slippage_bps).await?;
    println!();

    // Test 3: Best Price (compares both aggregators)
    if client.config().is_titan_configured() {
        println!("=== Test 3: Best Price Strategy ===");
        test_best_price(&client, SOL_TOKEN, USDC_TOKEN, sol_amount, slippage_bps).await?;
        println!();
    } else {
        println!("=== Test 3: Best Price Strategy ===");
        println!("⚠ Skipped: Titan not configured (requires both aggregators)\n");
    }

    // Test 4: Staircase (LowestSlippageClimber)
    if client.config().is_titan_configured() {
        println!("=== Test 4: Staircase Strategy (LowestSlippageClimber) ===");
        test_staircase(&client, SOL_TOKEN, USDC_TOKEN, sol_amount).await?;
        println!();
    } else {
        println!("=== Test 4: Staircase Strategy ===");
        println!("⚠ Skipped: Titan not configured (requires both aggregators)\n");
    }

    println!("=== All Tests Completed Successfully ===");
    Ok(())
}

/// Test simple Titan swap
async fn test_titan_swap(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    _slippage_bps: u16,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Titan,
            simulate: false, // Direct swap
        }),
        ..Default::default()
    };

    let result = client
        .swap_with_route_config(input, output, amount, route_config)
        .await?;

    println!("  ✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output Amount: {} lamports", result.out_amount);

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Titan,
            simulate: false,
        }),
        ..Default::default()
    };

    let result_back = client
        .swap_with_route_config(output, input, result.out_amount, route_config_back)
        .await?;

    println!("  ✓ Swap back successful!");
    println!("  Transaction: {}", result_back.signature);
    println!("  Output Amount: {} lamports", result_back.out_amount);

    Ok(())
}

/// Test simple Jupiter swap
async fn test_jupiter_swap(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    _slippage_bps: u16,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Jupiter,
            simulate: false, // Direct swap
        }),
        ..Default::default()
    };

    let result = client
        .swap_with_route_config(input, output, amount, route_config)
        .await?;

    println!("  ✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output Amount: {} lamports", result.out_amount);

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Jupiter,
            simulate: false,
        }),
        ..Default::default()
    };

    let result_back = client
        .swap_with_route_config(output, input, result.out_amount, route_config_back)
        .await?;

    println!("  ✓ Swap back successful!");
    println!("  Transaction: {}", result_back.signature);
    println!("  Output Amount: {} lamports", result_back.out_amount);

    Ok(())
}

/// Test best price strategy (compares both aggregators)
async fn test_best_price(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    _slippage_bps: u16,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);
    println!("  Strategy: BestPrice (compares all aggregators)");

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        ..Default::default()
    };

    let result = client
        .swap_with_route_config(input, output, amount, route_config)
        .await?;

    println!("  ✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output Amount: {} lamports", result.out_amount);

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        ..Default::default()
    };

    let result_back = client
        .swap_with_route_config(output, input, result.out_amount, route_config_back)
        .await?;

    println!("  ✓ Swap back successful!");
    println!("  Transaction: {}", result_back.signature);
    println!("  Output Amount: {} lamports", result_back.out_amount);

    Ok(())
}

/// Test staircase strategy (LowestSlippageClimber)
async fn test_staircase(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);
    println!("  Strategy: LowestSlippageClimber (tests multiple slippage levels)");

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::LowestSlippageClimber {
            floor_slippage_bps: 10, // Start at 0.1%
            max_slippage_bps: 100,  // Up to 1%
            step_bps: 10,           // Step by 0.1%
        }),
        ..Default::default()
    };

    let result = client
        .swap_with_route_config(input, output, amount, route_config)
        .await?;

    println!("  ✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output Amount: {} lamports", result.out_amount);

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::LowestSlippageClimber {
            floor_slippage_bps: 10,
            max_slippage_bps: 100,
            step_bps: 10,
        }),
        ..Default::default()
    };

    let result_back = client
        .swap_with_route_config(output, input, result.out_amount, route_config_back)
        .await?;

    println!("  ✓ Swap back successful!");
    println!("  Transaction: {}", result_back.signature);
    println!("  Output Amount: {} lamports", result_back.out_amount);

    Ok(())
}

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
//! - `DEX_SUPERAGG_TITAN__TITAN_API_KEY`: Titan API key (required for Titan tests)
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
    aggregators::QuoteResult, client::DexSuperAggClient, config::Aggregator, config::ClientConfig,
    config::RouteConfig, config::RoutingStrategy,
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

struct AggregatorTimings {
    jupiter_quote: Option<Duration>,
    titan_quote: Option<Duration>,
    dflow_quote: Option<Duration>,
    jupiter_exec: Option<Duration>,
    titan_exec: Option<Duration>,
    dflow_exec: Option<Duration>,
}

/// Timing summary for a test
#[derive(Default)]
struct TestTimingSummary {
    titan_forward: Option<(Option<Duration>, Option<Duration>)>, // (quote_time, exec_time)
    titan_back: Option<(Option<Duration>, Option<Duration>)>,
    jupiter_forward: Option<(Option<Duration>, Option<Duration>)>,
    jupiter_back: Option<(Option<Duration>, Option<Duration>)>,
    dflow_forward: Option<(Option<Duration>, Option<Duration>)>,
    dflow_back: Option<(Option<Duration>, Option<Duration>)>,
    best_price_forward: Option<AggregatorTimings>,
    best_price_back: Option<AggregatorTimings>,
    staircase_forward: Option<AggregatorTimings>,
    staircase_back: Option<AggregatorTimings>,
}

fn format_duration_ms(d: Option<Duration>) -> String {
    match d {
        Some(dur) => format!("{:.2} ms", dur.as_secs_f64() * 1000.0),
        None => "N/A".to_string(),
    }
}

fn extract_timing_from_quote_results(
    quote_results: &[(Aggregator, QuoteResult)],
    aggregator: Aggregator,
) -> Option<Duration> {
    quote_results
        .iter()
        .find(|(agg, _)| *agg == aggregator)
        .map(|(_, quote)| quote.quote_time)
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore] // Ignore by default - requires environment setup
async fn test_all_swap_methods() -> Result<()> {
    println!("=== Integration Test: All Swap Methods ===\n");

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

    let mut timing_summary = TestTimingSummary::default();

    // Test 1: Simple Titan Swap
    if client.config().is_titan_configured() {
        println!("=== Test 1: Simple Titan Swap ===");
        test_titan_swap(
            &client,
            SOL_TOKEN,
            USDC_TOKEN,
            sol_amount,
            slippage_bps,
            &mut timing_summary,
        )
        .await?;
        println!();
    } else {
        println!("=== Test 1: Simple Titan Swap ===");
        println!("⚠ Skipped: Titan not configured\n");
    }

    // Test 2: Simple Jupiter Swap
    println!("=== Test 2: Simple Jupiter Swap ===");
    test_jupiter_swap(
        &client,
        SOL_TOKEN,
        USDC_TOKEN,
        sol_amount,
        slippage_bps,
        &mut timing_summary,
    )
    .await?;
    println!();

    // Test 2.5: Simple DFlow Swap
    if client.config().is_dflow_configured() {
        println!("=== Test 2.5: Simple DFlow Swap ===");
        test_dflow_swap(
            &client,
            SOL_TOKEN,
            USDC_TOKEN,
            sol_amount,
            slippage_bps,
            &mut timing_summary,
        )
        .await?;
        println!();
    } else {
        println!("=== Test 2.5: Simple DFlow Swap ===");
        println!("⚠ Skipped: DFlow not configured\n");
    }

    // Test 3: Best Price (compares available aggregators)
    //
    // NOTE: This test requires Titan to be configured because the original goal was to compare
    // Jupiter vs Titan. If DFlow is configured too, it will also be included in BestPrice.
    if client.config().is_titan_configured() {
        println!("=== Test 3: Best Price Strategy ===");
        test_best_price(
            &client,
            SOL_TOKEN,
            USDC_TOKEN,
            sol_amount,
            slippage_bps,
            &mut timing_summary,
        )
        .await?;
        println!();
    } else {
        println!("=== Test 3: Best Price Strategy ===");
        println!("⚠ Skipped: Titan not configured\n");
    }

    // Test 4: Staircase (LowestSlippageClimber)
    //
    // NOTE: Staircase uses the same underlying quote mechanisms and will include DFlow if configured.
    if client.config().is_titan_configured() {
        println!("=== Test 4: Staircase Strategy (LowestSlippageClimber) ===");
        test_staircase(
            &client,
            SOL_TOKEN,
            USDC_TOKEN,
            sol_amount,
            &mut timing_summary,
        )
        .await?;
        println!();
    } else {
        println!("=== Test 4: Staircase Strategy ===");
        println!("⚠ Skipped: Titan not configured\n");
    }

    // Print timing summary
    println!("=== Timing Summary ===");
    println!();

    if let Some((sim, exec)) = timing_summary.titan_forward {
        println!("Titan Swap (Forward):");
        println!("  Quote: {}", format_duration_ms(sim));
        println!("  Execution: {}", format_duration_ms(exec));
        println!();
    }

    if let Some((sim, exec)) = timing_summary.titan_back {
        println!("Titan Swap (Back):");
        println!("  Quote: {}", format_duration_ms(sim));
        println!("  Execution: {}", format_duration_ms(exec));
        println!();
    }

    if let Some((sim, exec)) = timing_summary.jupiter_forward {
        println!("Jupiter Swap (Forward):");
        println!("  Quote: {}", format_duration_ms(sim));
        println!("  Execution: {}", format_duration_ms(exec));
        println!();
    }

    if let Some((sim, exec)) = timing_summary.jupiter_back {
        println!("Jupiter Swap (Back):");
        println!("  Quote: {}", format_duration_ms(sim));
        println!("  Execution: {}", format_duration_ms(exec));
        println!();
    }

    if let Some((sim, exec)) = timing_summary.dflow_forward {
        println!("DFlow Swap (Forward):");
        println!("  Quote: {}", format_duration_ms(sim));
        println!("  Execution: {}", format_duration_ms(exec));
        println!();
    }

    if let Some((sim, exec)) = timing_summary.dflow_back {
        println!("DFlow Swap (Back):");
        println!("  Quote: {}", format_duration_ms(sim));
        println!("  Execution: {}", format_duration_ms(exec));
        println!();
    }

    if let Some(timings) = timing_summary.best_price_forward {
        println!("Best Price Strategy (Forward):");
        println!(
            "  Jupiter Quote: {}",
            format_duration_ms(timings.jupiter_quote)
        );
        println!("  Titan Quote: {}", format_duration_ms(timings.titan_quote));
        println!("  DFlow Quote: {}", format_duration_ms(timings.dflow_quote));
        println!(
            "  Jupiter Execution: {}",
            format_duration_ms(timings.jupiter_exec)
        );
        println!(
            "  Titan Execution: {}",
            format_duration_ms(timings.titan_exec)
        );
        println!(
            "  DFlow Execution: {}",
            format_duration_ms(timings.dflow_exec)
        );
        println!();
    }

    if let Some(timings) = timing_summary.best_price_back {
        println!("Best Price Strategy (Back):");
        println!(
            "  Jupiter Quote: {}",
            format_duration_ms(timings.jupiter_quote)
        );
        println!("  Titan Quote: {}", format_duration_ms(timings.titan_quote));
        println!("  DFlow Quote: {}", format_duration_ms(timings.dflow_quote));
        println!(
            "  Jupiter Execution: {}",
            format_duration_ms(timings.jupiter_exec)
        );
        println!(
            "  Titan Execution: {}",
            format_duration_ms(timings.titan_exec)
        );
        println!(
            "  DFlow Execution: {}",
            format_duration_ms(timings.dflow_exec)
        );
        println!();
    }

    if let Some(timings) = timing_summary.staircase_forward {
        println!("Staircase Strategy (Forward):");
        println!(
            "  Jupiter Quote: {}",
            format_duration_ms(timings.jupiter_quote)
        );
        println!("  Titan Quote: {}", format_duration_ms(timings.titan_quote));
        println!("  DFlow Quote: {}", format_duration_ms(timings.dflow_quote));
        println!(
            "  Jupiter Execution: {}",
            format_duration_ms(timings.jupiter_exec)
        );
        println!(
            "  Titan Execution: {}",
            format_duration_ms(timings.titan_exec)
        );
        println!(
            "  DFlow Execution: {}",
            format_duration_ms(timings.dflow_exec)
        );
        println!();
    }

    if let Some(timings) = timing_summary.staircase_back {
        println!("Staircase Strategy (Back):");
        println!(
            "  Jupiter Quote: {}",
            format_duration_ms(timings.jupiter_quote)
        );
        println!("  Titan Quote: {}", format_duration_ms(timings.titan_quote));
        println!("  DFlow Quote: {}", format_duration_ms(timings.dflow_quote));
        println!(
            "  Jupiter Execution: {}",
            format_duration_ms(timings.jupiter_exec)
        );
        println!(
            "  Titan Execution: {}",
            format_duration_ms(timings.titan_exec)
        );
        println!(
            "  DFlow Execution: {}",
            format_duration_ms(timings.dflow_exec)
        );
        println!();
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
    slippage_bps: u16,
    timing_summary: &mut TestTimingSummary,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Titan,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary = client
        .swap_with_route_config(input, output, amount, route_config, true)
        .await?;

    // Collect timing data
    let quote_time = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Titan);
    let exec_time = summary.swap_result.execution_time;
    timing_summary.titan_forward = Some((quote_time, exec_time));

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

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Titan,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary_back = client
        .swap_with_route_config(
            output,
            input,
            summary.swap_result.out_amount,
            route_config_back,
            true,
        )
        .await?;

    // Collect timing data for swap back
    let quote_time_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Titan);
    let exec_time_back = summary_back.swap_result.execution_time;
    timing_summary.titan_back = Some((quote_time_back, exec_time_back));

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

    Ok(())
}

/// Test simple Jupiter swap
async fn test_jupiter_swap(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    slippage_bps: u16,
    timing_summary: &mut TestTimingSummary,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Jupiter,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary = client
        .swap_with_route_config(input, output, amount, route_config, true)
        .await?;

    // Collect timing data
    let quote_time = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Jupiter);
    let exec_time = summary.swap_result.execution_time;
    timing_summary.jupiter_forward = Some((quote_time, exec_time));

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

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Jupiter,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary_back = client
        .swap_with_route_config(
            output,
            input,
            summary.swap_result.out_amount,
            route_config_back,
            true,
        )
        .await?;

    // Collect timing data for swap back
    let quote_time_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Jupiter);
    let exec_time_back = summary_back.swap_result.execution_time;
    timing_summary.jupiter_back = Some((quote_time_back, exec_time_back));

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

    Ok(())
}

/// Test simple DFlow swap
async fn test_dflow_swap(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    slippage_bps: u16,
    timing_summary: &mut TestTimingSummary,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Dflow,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary = client
        .swap_with_route_config(input, output, amount, route_config, true)
        .await?;

    // Collect timing data
    let quote_time = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Dflow);
    let exec_time = summary.swap_result.execution_time;
    timing_summary.dflow_forward = Some((quote_time, exec_time));

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

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Dflow,
        }),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary_back = client
        .swap_with_route_config(
            output,
            input,
            summary.swap_result.out_amount,
            route_config_back,
            true,
        )
        .await?;

    // Collect timing data for swap back
    let quote_time_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Dflow);
    let exec_time_back = summary_back.swap_result.execution_time;
    timing_summary.dflow_back = Some((quote_time_back, exec_time_back));

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

    Ok(())
}

/// Test best price strategy (compares both aggregators)
async fn test_best_price(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    slippage_bps: u16,
    timing_summary: &mut TestTimingSummary,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);
    println!("  Strategy: BestPrice (compares all aggregators)");

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary = client
        .swap_with_route_config(input, output, amount, route_config, true)
        .await?;

    // Collect timing data
    let jup_quote = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Jupiter);
    let tit_quote = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Titan);
    let dflow_quote = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Dflow);
    let jup_exec = if summary.swap_result.aggregator_used == Some(Aggregator::Jupiter) {
        summary.swap_result.execution_time
    } else {
        None
    };
    let tit_exec = if summary.swap_result.aggregator_used == Some(Aggregator::Titan) {
        summary.swap_result.execution_time
    } else {
        None
    };
    let dflow_exec = if summary.swap_result.aggregator_used == Some(Aggregator::Dflow) {
        summary.swap_result.execution_time
    } else {
        None
    };
    timing_summary.best_price_forward = Some(AggregatorTimings {
        jupiter_quote: jup_quote,
        titan_quote: tit_quote,
        dflow_quote,
        jupiter_exec: jup_exec,
        titan_exec: tit_exec,
        dflow_exec,
    });

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

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        slippage_bps: Some(slippage_bps),
        ..Default::default()
    };

    let summary_back = client
        .swap_with_route_config(
            output,
            input,
            summary.swap_result.out_amount,
            route_config_back,
        )
        .await?;

    // Collect timing data for swap back
    let jup_quote_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Jupiter);
    let tit_quote_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Titan);
    let dflow_quote_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Dflow);
    let jup_exec_back = if summary_back.swap_result.aggregator_used == Some(Aggregator::Jupiter) {
        summary_back.swap_result.execution_time
    } else {
        None
    };
    let tit_exec_back = if summary_back.swap_result.aggregator_used == Some(Aggregator::Titan) {
        summary_back.swap_result.execution_time
    } else {
        None
    };
    let dflow_exec_back = if summary_back.swap_result.aggregator_used == Some(Aggregator::Dflow) {
        summary_back.swap_result.execution_time
    } else {
        None
    };
    timing_summary.best_price_back = Some(AggregatorTimings {
        jupiter_quote: jup_quote_back,
        titan_quote: tit_quote_back,
        dflow_quote: dflow_quote_back,
        jupiter_exec: jup_exec_back,
        titan_exec: tit_exec_back,
        dflow_exec: dflow_exec_back,
    });

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

    Ok(())
}

/// Test staircase strategy (LowestSlippageClimber)
async fn test_staircase(
    client: &DexSuperAggClient,
    input: &str,
    output: &str,
    amount: u64,
    timing_summary: &mut TestTimingSummary,
) -> Result<()> {
    println!("Swapping {} lamports of {} -> {}", amount, input, output);
    println!("  Strategy: LowestSlippageClimber (tests multiple slippage levels)");

    let floor_slippage_bps = 10; // Start at 0.1%
    let max_slippage_bps = 100; // Up to 1%
    let step_bps = 10; // Step by 0.1%

    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::LowestSlippageClimber {
            floor_slippage_bps,
            max_slippage_bps,
            step_bps,
        }),
        ..Default::default()
    };

    let summary = client
        .swap_with_route_config(input, output, amount, route_config)
        .await?;

    // Collect timing data
    let jup_quote = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Jupiter);
    let tit_quote = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Titan);
    let dflow_quote = extract_timing_from_quote_results(&summary.quote_results, Aggregator::Dflow);
    let jup_exec = if summary.swap_result.aggregator_used == Some(Aggregator::Jupiter) {
        summary.swap_result.execution_time
    } else {
        None
    };
    let tit_exec = if summary.swap_result.aggregator_used == Some(Aggregator::Titan) {
        summary.swap_result.execution_time
    } else {
        None
    };
    let dflow_exec = if summary.swap_result.aggregator_used == Some(Aggregator::Dflow) {
        summary.swap_result.execution_time
    } else {
        None
    };
    timing_summary.staircase_forward = Some(AggregatorTimings {
        jupiter_quote: jup_quote,
        titan_quote: tit_quote,
        dflow_quote,
        jupiter_exec: jup_exec,
        titan_exec: tit_exec,
        dflow_exec,
    });

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
            "  Final Slippage Used: {} bps ({:.2}%)",
            slippage_used,
            slippage_used as f64 / 100.0
        );

        // Show if slippage climbed higher than expected
        if slippage_used > floor_slippage_bps {
            let excess = slippage_used - floor_slippage_bps;
            println!(
                "  ⚠ Note: Required {} bps more than floor slippage ({} bps)",
                excess, floor_slippage_bps
            );
        } else {
            println!("  ✓ Succeeded at floor slippage - optimal!");
        }

        // Validate slippage used is within the expected range
        assert!(
            slippage_used >= floor_slippage_bps && slippage_used <= max_slippage_bps,
            "Slippage used ({}) should be between {} and {} bps",
            slippage_used,
            floor_slippage_bps,
            max_slippage_bps
        );
    }

    // Wait a bit for transaction to settle
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Swap back
    println!("\n  Swapping back: {} -> {}", output, input);
    let route_config_back = RouteConfig {
        routing_strategy: Some(RoutingStrategy::LowestSlippageClimber {
            floor_slippage_bps,
            max_slippage_bps,
            step_bps,
        }),
        ..Default::default()
    };

    let summary_back = client
        .swap_with_route_config(
            output,
            input,
            summary.swap_result.out_amount,
            route_config_back,
        )
        .await?;

    // Collect timing data for swap back
    let jup_quote_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Jupiter);
    let tit_quote_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Titan);
    let dflow_quote_back =
        extract_timing_from_quote_results(&summary_back.quote_results, Aggregator::Dflow);
    let jup_exec_back = if summary_back.swap_result.aggregator_used == Some(Aggregator::Jupiter) {
        summary_back.swap_result.execution_time
    } else {
        None
    };
    let tit_exec_back = if summary_back.swap_result.aggregator_used == Some(Aggregator::Titan) {
        summary_back.swap_result.execution_time
    } else {
        None
    };
    let dflow_exec_back = if summary_back.swap_result.aggregator_used == Some(Aggregator::Dflow) {
        summary_back.swap_result.execution_time
    } else {
        None
    };
    timing_summary.staircase_back = Some(AggregatorTimings {
        jupiter_quote: jup_quote_back,
        titan_quote: tit_quote_back,
        dflow_quote: dflow_quote_back,
        jupiter_exec: jup_exec_back,
        titan_exec: tit_exec_back,
        dflow_exec: dflow_exec_back,
    });

    println!("  ✓ Swap back successful!");
    println!("  Transaction: {}", summary_back.swap_result.signature);
    println!(
        "  Output Amount: {} lamports",
        summary_back.swap_result.out_amount
    );
    if let Some(slippage_used) = summary_back.swap_result.slippage_bps_used {
        println!(
            "  Final Slippage Used: {} bps ({:.2}%)",
            slippage_used,
            slippage_used as f64 / 100.0
        );

        // Show if slippage climbed higher than expected
        if slippage_used > floor_slippage_bps {
            let excess = slippage_used - floor_slippage_bps;
            println!(
                "  ⚠ Note: Required {} bps more than floor slippage ({} bps)",
                excess, floor_slippage_bps
            );
        } else {
            println!("  ✓ Succeeded at floor slippage - optimal!");
        }

        // Validate slippage used is within the expected range
        assert!(
            slippage_used >= floor_slippage_bps && slippage_used <= max_slippage_bps,
            "Slippage used ({}) should be between {} and {} bps",
            slippage_used,
            floor_slippage_bps,
            max_slippage_bps
        );
    }

    Ok(())
}

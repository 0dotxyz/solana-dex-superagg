# solana-dex-superagg

A simple Rust library for accessing **Titan** and **Jupiter** DEX aggregators on Solana. Easily plug and play into your code to get the best swap prices across multiple aggregators.

## Features

- **Multi-aggregator support**: Access both Titan and Jupiter from a single interface
- **Smart routing**: Automatically compare aggregators and use the one with the best price
- **Flexible strategies**: Choose from multiple routing strategies including best price, preferred aggregator, or lowest slippage climber
- **Slippage tracking**: Know exactly what slippage was used for your swaps (especially useful for staircase strategy)
- **Aggregator tracking**: See which aggregator was selected for each swap

## Environment Variables

Set the following environment variables before using the library:

```bash
# Solana RPC endpoint
export DEX_SUPERAGG_SHARED__RPC_URL="https://api.mainnet-beta.solana.com"

# Wallet keypair (base58 string, JSON array, or comma-separated bytes)
export DEX_SUPERAGG_SHARED__WALLET_KEYPAIR="your_keypair_here"

# Jupiter Configuration
# Jupiter API URL (defaults to https://api.jup.ag/swap/v1 if not set)
export DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL="https://api.jup.ag/swap/v1"

# Jupiter API key (optional but recommended for production)
# Get your API key from https://portal.jup.ag/
export DEX_SUPERAGG_JUPITER__API_KEY="your_jupiter_api_key"

# Titan Configuration (Optional)
# Titan WebSocket endpoint
export DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT="us1.api.demo.titan.exchange"

# Titan API key (required)
export DEX_SUPERAGG_TITAN__TITAN_API_KEY="your_titan_api_key"

# Optional Configuration
# Slippage tolerance in basis points (default: 50 = 0.5%)
export DEX_SUPERAGG_SHARED__SLIPPAGE_BPS="25"

# Compute unit price in micro lamports (default: 0)
export DEX_SUPERAGG_SHARED__COMPUTE_UNIT_PRICE_MICRO_LAMPORTS="5000"

# Default routing strategy (default: best_price)
# Options: best_price, preferred_aggregator, lowest_slippage_climber
export DEX_SUPERAGG_SHARED__ROUTING_STRATEGY__TYPE="best_price"
```

## Quick Start

### Create the Client

```rust
use solana_dex_superagg::{client::DexSuperAggClient, config::ClientConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration from environment variables
    let config = ClientConfig::from_env()?;
    
    // Validate configuration (optional but recommended)
    if let Err(errors) = config.validate().await {
        eprintln!("Configuration errors:");
        for error in errors {
            eprintln!("  - {}", error);
        }
        return Err(anyhow::anyhow!("Invalid configuration"));
    }
    
    // Create the client
    let client = DexSuperAggClient::new(config)?;
    
    // Use the client for swaps...
    
    // Clean up when done (closes Titan WebSocket connection if used)
    client.close().await?;
    
    Ok(())
}
```

## Routing Strategies

The library supports three routing strategies:

- **BestPrice** (default): Automatically compares all available aggregators and uses the one that gives you the most tokens
- **PreferredAggregator**: Use a specific aggregator (Jupiter or Titan)
- **LowestSlippageClimber**: Tests multiple slippage levels starting from a floor value and incrementing until a swap succeeds

## SwapResult

The `SwapResult` struct contains:

```rust
pub struct SwapResult {
    /// Transaction signature of the executed swap
    pub signature: String,
    /// Output amount received (in lamports/base units)
    pub out_amount: u64,
    /// Slippage tolerance actually used in basis points
    /// This is particularly useful for LowestSlippageClimber strategy
    pub slippage_bps_used: Option<u16>,
    /// Aggregator that was used for this swap (Jupiter or Titan)
    /// This is particularly useful for BestPrice and LowestSlippageClimber strategies
    pub aggregator_used: Option<Aggregator>,
}
```

## Complete Example

```rust
use solana_dex_superagg::{
    client::DexSuperAggClient,
    config::{ClientConfig, RouteConfig, RoutingStrategy, Aggregator},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config from environment variables
    let config = ClientConfig::from_env()?;
    
    // Validate config (optional but recommended)
    config.validate().await?;
    
    // Create client
    let client = DexSuperAggClient::new(config)?;
    
    // Token addresses
    let sol = "So11111111111111111111111111111111111111112";
    let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    let amount = 1_000_000_000; // 1 SOL in lamports
    
    // Example 1: Simple swap (uses default strategy from config - usually BestPrice)
    // This is the easiest way to swap - no configuration needed!
    println!("=== Simple Swap (Default Strategy) ===");
    let result = client.swap(sol, usdc, amount).await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output: {} lamports", result.out_amount);
    if let Some(agg) = result.aggregator_used {
        println!("  Aggregator: {:?}", agg);
    }
    
    // Example 2: Best Price Strategy (explicitly compare all aggregators)
    // This compares Jupiter and Titan (if configured) and uses whichever gives more tokens
    println!("\n=== Best Price Strategy ===");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::BestPrice),
        slippage_bps: Some(25), // Override default slippage to 0.25%
        ..Default::default()
    };
    let result = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output: {} lamports", result.out_amount);
    if let Some(agg) = result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    if let Some(slippage) = result.slippage_bps_used {
        println!("  Slippage used: {} bps ({:.2}%)", slippage, slippage as f64 / 100.0);
    }
    
    // Example 3: Preferred Aggregator - Use Jupiter
    // Force the swap to use Jupiter instead of comparing aggregators
    println!("\n=== Preferred Aggregator (Jupiter) ===");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Jupiter,
            simulate: false, // Set to true to simulate before executing
        }),
        slippage_bps: Some(25),
        ..Default::default()
    };
    let result = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output: {} lamports", result.out_amount);
    if let Some(agg) = result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    
    // Example 4: Preferred Aggregator - Use Titan
    // Force the swap to use Titan instead of comparing aggregators
    println!("\n=== Preferred Aggregator (Titan) ===");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::PreferredAggregator {
            aggregator: Aggregator::Titan,
            simulate: false,
        }),
        slippage_bps: Some(25),
        ..Default::default()
    };
    let result = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output: {} lamports", result.out_amount);
    if let Some(agg) = result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    
    // Example 5: Lowest Slippage Climber (Staircase Strategy)
    // Tests multiple slippage levels starting from floor_slippage_bps and incrementing by step_bps
    // until a swap succeeds or max_slippage_bps is reached
    // This is useful for finding the minimum slippage needed for a swap
    println!("\n=== Lowest Slippage Climber (Staircase) ===");
    let route_config = RouteConfig {
        routing_strategy: Some(RoutingStrategy::LowestSlippageClimber {
            floor_slippage_bps: 10,  // Start at 0.1%
            max_slippage_bps: 100,   // Up to 1%
            step_bps: 10,            // Step by 0.1%
        }),
        ..Default::default()
    };
    let result = client
        .swap_with_route_config(sol, usdc, amount, route_config)
        .await?;
    println!("✓ Swap successful!");
    println!("  Transaction: {}", result.signature);
    println!("  Output: {} lamports", result.out_amount);
    if let Some(agg) = result.aggregator_used {
        println!("  Aggregator used: {:?}", agg);
    }
    if let Some(slippage) = result.slippage_bps_used {
        println!("  Slippage used: {} bps ({:.2}%)", slippage, slippage as f64 / 100.0);
        // The staircase strategy will warn if slippage climbed higher than the floor
    }
    
    // Clean up when done (closes Titan WebSocket connection if used)
    client.close().await?;
    
    Ok(())
}
```

## Notes

- **Token amounts**: All amounts are in lamports/base units (not human-readable). For example, 1 SOL = 1,000,000,000 lamports, 1 USDC = 1,000,000 lamports.
- **Slippage**: Slippage is specified in basis points (bps). 1 bps = 0.01%, so 25 bps = 0.25%.
- **Titan WebSocket**: The Titan aggregator maintains a persistent WebSocket connection that is reused across swaps for efficiency. Make sure to call `client.close().await?` when you're done to clean up resources.
- **ATA Creation**: By default, the client will create Associated Token Accounts (ATAs) for output tokens if they don't exist. You can control this behavior via `RouteConfig::output_ata`.

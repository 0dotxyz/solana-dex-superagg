# AI Overview - Solana DEX Super Aggregator

## Project Purpose

**solana-dex-superagg** is a Rust library designed to be **imported and used as a dependency** in other Solana projects. The primary goal is to provide a simple, unified interface for swapping tokens across multiple DEX aggregators (currently Jupiter and Titan) with automatic price comparison and routing.

## Core Philosophy

The library enables users to:
- **Easily swap tokens** without worrying about which aggregator to use
- **Automatically get the best price** by comparing quotes from Jupiter and Titan
- **Use a single, consistent API** regardless of which aggregator executes the swap

## Project Structure

```
solana-dex-superagg/
├── src/
│   ├── lib.rs          # Main library exports
│   ├── config.rs       # Configuration system (shared, Jupiter, Titan)
│   └── client.rs       # Main DexSuperAggClient
├── Cargo.toml          # Package definition
└── README.md           # User-facing documentation
```

## Key Components

### 1. Configuration System (`config.rs`)

The configuration system uses a hierarchical structure:

- **SharedConfig**: Common settings used by all aggregators
  - RPC URL
  - Slippage tolerance (basis points)
  - Wallet keypair (optional - for quote-only operations)
  - Compute unit price

- **JupiterConfig**: Jupiter-specific settings
  - Jupiter Swap API URL

- **TitanConfig**: Titan-specific settings (optional)
  - Titan WebSocket endpoint
  - Titan API key (for direct mode)
  - Hermes endpoint (alternative to direct mode)

- **ClientConfig**: Combines all configurations
  - Loads from environment variables using `DEX_SUPERAGG_` prefix
  - Validates connectivity to endpoints
  - Supports optional Titan configuration

**Configuration Loading Pattern:**
```rust
// Load from environment variables
let config = ClientConfig::from_env()?;

// Or create manually
let config = ClientConfig {
    shared: SharedConfig { ... },
    jupiter: JupiterConfig { ... },
    titan: Some(TitanConfig { ... }),
};
```

### 2. Client (`client.rs`)

**DexSuperAggClient** is the main entry point for users:

- Requires a wallet keypair (for executing swaps)
- Provides access to RPC client and signer
- Currently provides basic initialization and accessors
- **Future**: Will implement swap methods that compare prices and route to best aggregator

**Usage Pattern:**
```rust
// Create client from environment
let client = DexSuperAggClient::from_env()?;

// Or create from config
let config = ClientConfig::from_env()?;
let client = DexSuperAggClient::new(config)?;
```

## Design Goals

### 1. Library-First Design

This is **not a CLI tool** - it's a library meant to be imported:

```toml
[dependencies]
solana-dex-superagg = { path = "../solana-dex-superagg" }
```

### 2. Price Comparison & Routing

The core functionality should:
- Fetch quotes from both Jupiter and Titan (if configured)
- Compare output amounts
- Automatically route to the aggregator with the best price
- Provide fallback if one aggregator fails

**Intended API (to be implemented):**
```rust
// Get best quote across all aggregators
let quote = client.get_best_quote(input_mint, output_mint, amount).await?;

// Execute swap using best aggregator
let signature = client.swap(input_mint, output_mint, amount).await?;

// Or get quotes from all aggregators for comparison
let quotes = client.get_all_quotes(input_mint, output_mint, amount).await?;
```

### 3. Flexibility

- **Optional Titan**: Library works with just Jupiter if Titan isn't configured
- **Quote-Only Mode**: Can be used for simulations without a wallet keypair
- **Configurable**: All settings can be overridden via environment variables

## Configuration Environment Variables

The library uses the `DEX_SUPERAGG_` prefix with `__` for nested keys:

```bash
# Shared configuration
DEX_SUPERAGG_SHARED__RPC_URL=https://api.mainnet-beta.solana.com
DEX_SUPERAGG_SHARED__SLIPPAGE_BPS=50
DEX_SUPERAGG_SHARED__WALLET_KEYPAIR=<base58_keypair>
DEX_SUPERAGG_SHARED__COMPUTE_UNIT_PRICE_MICRO_LAMPORTS=0

# Jupiter configuration
DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL=https://quote-api.jup.ag/v6

# Titan configuration (optional)
DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT=us1.api.demo.titan.exchange
DEX_SUPERAGG_TITAN__TITAN_API_KEY=<titan_api_key>
DEX_SUPERAGG_TITAN__HERMES_ENDPOINT=https://hermes.demo.titan.exchange
```

## Current State

### ✅ Implemented
- Configuration system with environment variable loading
- Configuration validation (endpoint connectivity, keypair parsing)
- Basic client structure
- Support for optional Titan configuration
- Comprehensive test coverage for configuration

### 🚧 To Be Implemented
- Quote fetching from Jupiter
- Quote fetching from Titan
- Price comparison logic
- Swap execution routing
- Error handling and fallback strategies
- Transaction building and signing
- Integration examples

## Integration Example (Future)

```rust
use solana_dex_superagg::{DexSuperAggClient, ClientConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config from environment
    let client = DexSuperAggClient::from_env()?;
    
    // Get best quote (compares Jupiter and Titan automatically)
    let quote = client
        .get_best_quote(
            "So11111111111111111111111111111111111111112", // SOL
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
            1_000_000_000, // 1 SOL
        )
        .await?;
    
    println!("Best quote: {} USDC from {}", quote.out_amount, quote.aggregator);
    
    // Execute swap using best aggregator
    let signature = client
        .swap(
            "So11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            1_000_000_000,
        )
        .await?;
    
    println!("Swap executed: {}", signature);
    Ok(())
}
```

## Key Principles for AI Assistants

1. **This is a library, not an application** - focus on API design and usability for importers
2. **Price comparison is core** - always compare quotes from all available aggregators
3. **Fail gracefully** - if one aggregator fails, fall back to others
4. **Configuration is flexible** - support optional Titan, optional wallet for quotes
5. **Follow Rust best practices** - proper error handling, async/await, clear documentation
6. **Test thoroughly** - configuration system has good test coverage, maintain this standard

## Related Projects

This library is designed to be used by projects like:
- Trading bots that need best execution
- DeFi applications requiring DEX aggregation
- Arbitrage systems comparing prices across venues
- Portfolio management tools executing swaps

## Notes for Future Development

- Consider adding more aggregators (e.g., Raydium, Orca) as the ecosystem grows
- Implement quote caching to reduce API calls
- Add support for limit orders and advanced order types
- Consider implementing a quote streaming API for real-time price updates
- Add comprehensive logging and metrics for monitoring swap performance


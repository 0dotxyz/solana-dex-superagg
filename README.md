# solana-dex-superagg

A simple Rust library for accessing **Jupiter**, **Titan**, and **DFlow** DEX aggregators on Solana. Easily plug and play into your code to get the best swap prices across multiple aggregators.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
solana-dex-superagg = { git = "https://github.com/0dotxyz/solana-dex-superagg" }
```

## Features

- **Multi-aggregator support**: Access Jupiter, Titan, and DFlow from a single interface
- **Smart routing**: Quotes every configured aggregator in parallel and builds the best quote; if that venue fails to build, falls back to the next best
- **Flexible strategies**: Choose from multiple routing strategies including best price, preferred aggregator, or lowest slippage climber
- **Slippage tracking**: Know exactly what slippage was used for your swaps (especially useful for staircase strategy)
- **Aggregator tracking**: See which aggregator was selected for each swap
- **Performance monitoring**: Built-in timing information for both simulations and executions
- **Configurable commitment levels**: Choose between Processed, Confirmed (default), or Finalized based on your needs

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

# DFlow Configuration (Optional)
# DFlow API URL (defaults to https://d.quote-api.dflow.net if not set)
export DEX_SUPERAGG_DFLOW__API_URL="https://d.quote-api.dflow.net"

# DFlow API key (optional - DFlow works without it but with rate limiting)
export DEX_SUPERAGG_DFLOW__API_KEY="your_dflow_api_key"

# Optional Configuration
# Slippage tolerance in basis points (default: 50 = 0.5%)
export DEX_SUPERAGG_SHARED__SLIPPAGE_BPS="25"

# Compute unit price in micro lamports (default: 0)
export DEX_SUPERAGG_SHARED__COMPUTE_UNIT_PRICE_MICRO_LAMPORTS="5000"

# Commitment level for transaction confirmation (default: confirmed)
# Options: processed, confirmed, finalized
# - processed: Fastest (~400ms), but can be rolled back
# - confirmed: Good balance (~1-2s), very unlikely to roll back (default)
# - finalized: Slowest (~15s), cannot be rolled back
export DEX_SUPERAGG_SHARED__COMMITMENT_LEVEL="confirmed"

# Per-venue quote deadline in ms (default: 5000). A venue that misses it is left out of the comparison.
export DEX_SUPERAGG_SHARED__QUOTE_TIMEOUT_MS="5000"

# Per-venue transaction build deadline in ms (default: 10000)
export DEX_SUPERAGG_SHARED__BUILD_TIMEOUT_MS="10000"

# How long `swap` waits for confirmation, in seconds (default: 90)
export DEX_SUPERAGG_SHARED__CONFIRM_TIMEOUT_SECS="90"

# Default routing strategy (default: best_price)
# Options: best_price, preferred_aggregator, lowest_slippage_climber
export DEX_SUPERAGG_SHARED__ROUTING_STRATEGY__TYPE="best_price"
```

## Quick Start

The easiest way to get started is to run the example code in the `examples/` directory:

```bash
DEX_SUPERAGG_SHARED__RPC_URL='https://api.mainnet-beta.solana.com' \
DEX_SUPERAGG_TITAN__TITAN_API_KEY='xxxxx' \
DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT='us1.api.demo.titan.exchange' \
DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL='https://api.jup.ag/swap/v1' \
DEX_SUPERAGG_JUPITER__API_KEY='xxxx' \
DEX_SUPERAGG_SHARED__WALLET_KEYPAIR='[1,2,3,4.....]' \
cargo run --example basic_swap
```

The `basic_swap` example demonstrates all features including:
- Simple swap with default strategy
- Best Price strategy with quote results
- Preferred aggregator (Jupiter and Titan)
- Lowest Slippage Climber (staircase strategy)
- Different commitment levels (Confirmed vs Finalized)

See `examples/basic_swap.rs` for the complete code.

## Routing Strategies

The library supports three routing strategies:

- **BestPrice** (default): Automatically compares all available aggregators and uses the one that gives you the most tokens
- **PreferredAggregator**: Use a specific aggregator (Jupiter, Titan or DFlow)
- **LowestSlippageClimber**: Tests multiple slippage levels starting from a floor value and incrementing until a swap succeeds

## Notes

- **Token amounts**: All amounts are in lamports/base units (not human-readable). For example, 1 SOL = 1,000,000,000 lamports, 1 USDC = 1,000,000 lamports.
- **Slippage**: Slippage is specified in basis points (bps). 1 bps = 0.01%, so 25 bps = 0.25%.
- **Commitment Level**: The commitment level determines how long to wait for transaction confirmation:
  - `Processed`: Fastest (~400ms), but can be rolled back
  - `Confirmed`: Good balance (~1-2s), very unlikely to roll back (default)
  - `Finalized`: Slowest (~15s), cannot be rolled back
  Default is `Confirmed` for a good balance of speed and reliability. Use `Finalized` only for critical swaps that require absolute guarantees.
- **SOL handling** (`wrap_and_unwrap_sol`, identical on every venue):
  - `false`: wSOL is an ordinary SPL token. A wSOL input is spent from the wallet's wSOL ATA (which must already exist), a wSOL output is left in it, and the ATA is never closed. Use this when your program keeps SOL as wSOL.
  - `true`: native SOL. A SOL input is wrapped from the wallet's lamports, a SOL output is unwrapped to lamports, and the wSOL ATA is closed afterwards — which also unwraps any wSOL already sitting in it.
- **Min-out swaps**: `build_swap_transaction_for_min_out*` quotes your initial input and, if no venue yields `min_out`, raises the input by the shortfall and re-quotes (at most twice) — never above `max_in_amount`. Pass the most you can afford to spend (e.g. from an oracle price); `None` caps it at 1.10x the initial amount.
- **Titan WebSocket**: One connection is shared by all callers and opened on first use. Concurrent quotes are routed by request/stream id, every request has a deadline, and a dropped or timed-out connection is replaced on the next request (a request that hits a dropped connection is retried once). Titan routes get a compute budget (`computeUnitsSafe` limit plus your CU price). Call `client.close().await?` when you're done.
- **ATA Creation**: By default, the client will create Associated Token Accounts (ATAs) for output tokens if they don't exist. You can control this behavior via `RouteConfig::output_ata`.
- **Timing Information**: Both `SwapResult` and `QuoteResult` include timing information (`execution_time` and `quote_time` respectively) to help you monitor performance. The `SwapSummary` includes all quote results, allowing you to see timing for all aggregators that were tried (useful for BestPrice strategy).

# Testing

To test this, simply use:

```bash
DEX_SUPERAGG_SHARED__RPC_URL='https://api.mainnet-beta.solana.com' \
DEX_SUPERAGG_TITAN__TITAN_API_KEY='xxxxx' \
DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT='us1.api.demo.titan.exchange' \
DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL='https://api.jup.ag/swap/v1' \
DEX_SUPERAGG_JUPITER__API_KEY='xxxx' \
DEX_SUPERAGG_SHARED__WALLET_KEYPAIR='[1,2,3,4.....]' \
cargo test test_all_swap -- --nocapture --ignored
```

# Adding New Aggregators

Want to integrate a new DEX aggregator (Raydium, Orca, Phoenix, etc.)? See **[AGGREGATOR_INTEGRATION.md](./AGGREGATOR_INTEGRATION.md)** for a complete step-by-step guide with:
- Code templates and examples
- Configuration setup
- Integration checklist (24 steps)
- Testing and verification

The library's architecture makes it straightforward to add new aggregators by implementing the `DexAggregator` trait.
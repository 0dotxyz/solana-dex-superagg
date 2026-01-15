# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

This is a Rust library (`solana-dex-superagg`) that provides a unified interface for accessing multiple Solana DEX aggregators (Jupiter, Titan, and DFlow) with intelligent routing strategies. The library compares quotes from different aggregators and automatically selects the best price or uses custom routing logic.

## Adding New Aggregators

**To add a new DEX aggregator (Raydium, Orca, Phoenix, etc.), see [AGGREGATOR_INTEGRATION.md](./AGGREGATOR_INTEGRATION.md)** for a complete step-by-step guide with code examples, checklists, and verification steps.

The integration process involves:
1. Creating the aggregator implementation (`src/aggregators/your_aggregator/mod.rs`)
2. Adding configuration structs and validation
3. Integrating into the client routing system
4. Testing and verification

## Building and Testing

**IMPORTANT**: There is a `.env` file in the repository root with all required environment variables pre-configured. You can source this file to avoid setting variables manually:

```bash
# Load environment variables from .env
source .env

# Or export them for the current shell session
export $(cat .env | xargs)
```

### Build
```bash
cargo build
```

### Run Tests
```bash
# Run all tests (unit tests)
cargo test

# Run integration test (requires environment variables from .env)
# Option 1: Source .env first
source .env
cargo test test_all_swap -- --nocapture --ignored

# Option 2: Set variables inline (if you don't want to source .env)
DEX_SUPERAGG_SHARED__RPC_URL='https://api.mainnet-beta.solana.com' \
DEX_SUPERAGG_TITAN__TITAN_API_KEY='xxxxx' \
DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT='us1.api.demo.titan.exchange' \
DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL='https://api.jup.ag/swap/v1' \
DEX_SUPERAGG_JUPITER__API_KEY='xxxx' \
DEX_SUPERAGG_SHARED__WALLET_KEYPAIR='[1,2,3,4.....]' \
cargo test test_all_swap -- --nocapture --ignored
```

### Run Examples
```bash
# Run basic_swap example demonstrating all routing strategies
# Option 1: Source .env first (recommended)
source .env
cargo run --example basic_swap

# Option 2: Set variables inline
DEX_SUPERAGG_SHARED__RPC_URL='https://api.mainnet-beta.solana.com' \
DEX_SUPERAGG_TITAN__TITAN_API_KEY='xxxxx' \
DEX_SUPERAGG_TITAN__TITAN_WS_ENDPOINT='us1.api.demo.titan.exchange' \
DEX_SUPERAGG_JUPITER__JUP_SWAP_API_URL='https://api.jup.ag/swap/v1' \
DEX_SUPERAGG_JUPITER__API_KEY='xxxx' \
DEX_SUPERAGG_SHARED__WALLET_KEYPAIR='[1,2,3,4.....]' \
cargo run --example basic_swap
```

## Architecture

### Core Components

1. **DexSuperAggClient** (`src/client.rs`)
   - Main entry point for users
   - Manages aggregator connections and routing logic
   - Lazily initializes and reuses Titan WebSocket connection to avoid reconnection overhead
   - Implements three routing strategies: BestPrice, PreferredAggregator, LowestSlippageClimber

2. **DexAggregator Trait** (`src/aggregators/mod.rs`)
   - Unified interface for all DEX aggregators
   - Two main operations: `swap()` and `simulate()`
   - Implemented by `JupiterAggregator` and `TitanAggregator`

3. **Configuration System** (`src/config.rs`)
   - Uses `figment` for flexible config loading from environment variables
   - Supports multiple wallet keypair formats: base58, JSON array, comma-separated
   - Custom deserializers for `CommitmentLevel` and `wallet_keypair`
   - Includes async validation methods that test actual connectivity to RPC/API endpoints

4. **Aggregator Implementations**
   - **Jupiter** (`src/aggregators/jupiter/mod.rs`): HTTP-based API client using `jupiter-swap-api-client`
   - **Titan** (`src/aggregators/titan/`): WebSocket-based client with custom MessagePack codec

### Routing Strategies

The library supports three distinct routing strategies defined in `config.rs`:

1. **BestPrice** (default)
   - Simulates swaps on all available aggregators in parallel
   - Selects the aggregator that returns the highest output amount
   - Returns `SwapSummary` with all simulation results for transparency

2. **PreferredAggregator**
   - Forces use of a specific aggregator (Jupiter or Titan)
   - Optional simulation before execution via `simulate` flag
   - Useful when you trust a specific aggregator or need deterministic routing

3. **LowestSlippageClimber** (staircase strategy)
   - Attempts swaps starting at `floor_slippage_bps` and incrementing by `step_bps`
   - Stops at first successful swap or when `max_slippage_bps` is reached
   - Returns the actual slippage used via `SwapResult.slippage_bps_used`
   - Logs warnings if slippage climbs higher than floor (indicates market conditions require higher slippage)

### Key Design Patterns

1. **Connection Reuse**: Titan WebSocket is initialized once and reused via `Arc<Mutex<Option<Arc<TitanAggregator>>>>` to avoid reconnection overhead

2. **ATA Management**: The client can automatically create Associated Token Accounts before swaps via `RouteConfig.output_ata` (defaults to `Create`)

3. **Commitment Levels**: Supports three Solana commitment levels with different speed/finality tradeoffs:
   - `Processed`: ~400ms, can be rolled back
   - `Confirmed`: ~1-2s, very unlikely to roll back (default)
   - `Finalized`: ~15s, cannot be rolled back

4. **Result Metadata**: Both `SwapResult` and `SimulateResult` include timing information and aggregator tracking for observability

## Important Implementation Details

### Configuration Loading
- Environment variables use `DEX_SUPERAGG_` prefix with `__` for nesting (e.g., `DEX_SUPERAGG_SHARED__RPC_URL`)
- Wallet keypair accepts three formats: base58 string, JSON array `[1,2,3,...]`, or comma-separated bytes
- Custom deserializer (`deserialize_wallet_keypair`) handles figment parsing JSON arrays from env vars

### Titan WebSocket
- Uses persistent connection initialized in `get_titan_aggregator()`
- Must call `client.close().await?` when done to clean up WebSocket
- Custom MessagePack codec in `src/aggregators/titan/codec.rs` for binary framing
- Transaction builder in `src/aggregators/titan/transaction_builder.rs` handles Titan-specific serialization

### Token Amounts
- All amounts are in lamports/base units (not human-readable)
- 1 SOL = 1,000,000,000 lamports
- 1 USDC = 1,000,000 lamports (6 decimals)

### Slippage
- Specified in basis points (bps): 1 bps = 0.01%
- Example: 25 bps = 0.25%, 100 bps = 1%

### Native SOL Handling
- Native SOL mint: `So11111111111111111111111111111111111111112`
- Does NOT create ATA for native SOL (uses native account balance)
- See `create_ata_if_needed()` in `src/client.rs:115-195`

## File Organization

```
src/
├── lib.rs                          # Public API exports
├── client.rs                       # DexSuperAggClient implementation
├── config.rs                       # Configuration types and loading
└── aggregators/
    ├── mod.rs                      # DexAggregator trait and shared types
    ├── jupiter/
    │   └── mod.rs                  # Jupiter HTTP client
    └── titan/
        ├── mod.rs                  # Public Titan exports
        ├── client.rs               # Titan WebSocket client
        ├── types.rs                # Titan request/response types
        ├── codec.rs                # MessagePack framing codec
        └── transaction_builder.rs # Titan transaction serialization

examples/
└── basic_swap.rs                   # Comprehensive usage examples

tests/
└── integration_swap_test.rs        # End-to-end swap test
```

## Dependencies

- **solana-sdk** / **solana-client** / **solana-program**: v2.1.20 (pinned for compatibility)
- **jupiter-swap-api-client**: From GitHub (rev: 6e209786084a4344538a1aa695f126169fa95022)
- **tokio-tungstenite**: For Titan WebSocket connection
- **figment**: For flexible configuration loading
- **rmp-serde**: MessagePack serialization for Titan

## Common Token Addresses

```rust
SOL:  "So11111111111111111111111111111111111111112"
USDC: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
```

## Version History (from git log)

- `4df2de4`: Clean Logging
- `9d24fae`: Add Timing To Return + Commitment Levels
- `7d9c395`: Downgrade package to avoid conflicts on import
- `05a5269`: How To Run Test
- `c52989a`: Versioning and import

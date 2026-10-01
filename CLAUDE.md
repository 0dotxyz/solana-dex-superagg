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
   - Main entry point for users; owns one instance of each configured venue
   - Quotes all venues of a strategy in parallel (`collect_quotes`, per-venue `quote_timeout_ms`),
     logs failures, sorts by output, then `build_best` builds the best quote (falling back to the
     next best on a build failure, under `build_timeout_ms`)
   - Min-out swaps raise the input by the shortfall ratio and re-quote, capped by the caller's
     `max_in_amount` (default 1.10x the initial amount)
   - `swap*` submits with the nonblocking RPC client and polls for the commitment level,
     rebroadcasting while the blockhash is valid

2. **DexAggregator Trait** (`src/aggregators/mod.rs`)
   - Two phases: `quote(&SwapRequest) -> Quote` and `build_transaction(&SwapRequest, &Quote)`;
     a `Quote` carries the venue payload so the compared quote is exactly what gets built
   - `SwapRequest::wrap_and_unwrap_sol` has one meaning on every venue (see its doc comment)
   - Implemented by `JupiterAggregator`, `TitanAggregator`, `DflowAggregator`

3. **Configuration System** (`src/config.rs`)
   - Uses `figment` for flexible config loading from environment variables
   - Supports multiple wallet keypair formats: base58, JSON array, comma-separated
   - Custom deserializers for `CommitmentLevel` and `wallet_keypair`
   - Includes async validation methods that test actual connectivity to RPC/API endpoints

4. **Aggregator Implementations**
   - **Jupiter** (`src/aggregators/jupiter/mod.rs`): HTTP via `jupiter-swap-api-client`;
     `dynamicComputeUnitLimit` on, simulation errors rejected
   - **Titan** (`src/aggregators/titan/`): WebSocket + MessagePack. Routes come back as bare
     instructions; we add the compute budget and native-SOL wrap and compile the tx
   - **DFlow** (`src/aggregators/dflow/mod.rs`): HTTP; passes `wrapAndUnwrapSol` and
     `computeUnitPriceMicroLamports`

### Routing Strategies

The library supports three distinct routing strategies defined in `config.rs`:

1. **BestPrice** (default)
   - Quotes swaps on all available aggregators in parallel
   - Selects the aggregator that returns the highest output amount
   - Returns `SwapSummary` with all quote results for transparency

2. **PreferredAggregator**
   - Forces use of a specific aggregator (Jupiter or Titan)
   - Executes swap via the selected aggregator (the returned `SwapSummary` also includes the quote used)
   - Useful when you trust a specific aggregator or need deterministic routing

3. **LowestSlippageClimber** (staircase strategy)
   - Attempts swaps starting at `floor_slippage_bps` and incrementing by `step_bps`
   - Stops at first successful swap or when `max_slippage_bps` is reached
   - Returns the actual slippage used via `SwapResult.slippage_bps_used`
   - Logs warnings if slippage climbs higher than floor (indicates market conditions require higher slippage)

### Key Design Patterns

1. **Titan connection**: one WebSocket shared by all callers, opened lazily. A reader task routes
   server messages by request id / stream id, so concurrent quotes can't consume each other's
   replies. Every request has a deadline; a dropped or timed-out connection is retired and replaced
   on the next request, and a request that hits a dropped connection is retried once.

2. **ATA Management**: `swap*` can create the output ATA first via `RouteConfig.output_ata`
   (defaults to `Create`)

3. **Commitment Levels**: Supports three Solana commitment levels with different speed/finality tradeoffs:
   - `Processed`: ~400ms, can be rolled back
   - `Confirmed`: ~1-2s, very unlikely to roll back (default)
   - `Finalized`: ~15s, cannot be rolled back

4. **Result Metadata**: Both `SwapResult` and `QuoteResult` include timing information and aggregator tracking for observability

## Important Implementation Details

### Configuration Loading
- Environment variables use `DEX_SUPERAGG_` prefix with `__` for nesting (e.g., `DEX_SUPERAGG_SHARED__RPC_URL`)
- Wallet keypair accepts three formats: base58 string, JSON array `[1,2,3,...]`, or comma-separated bytes
- Custom deserializer (`deserialize_wallet_keypair`) handles figment parsing JSON arrays from env vars

### Titan WebSocket
- `TitanClient` (`src/aggregators/titan/client.rs`) owns the connection; see Key Design Patterns
- Call `client.close().await?` when done to close the socket
- `transaction_builder.rs` assembles compute budget + optional wrap + route instructions and fetches
  lookup tables (a missing table is an error)
- Routes delivered only as a prebuilt `transaction` (no instructions) are skipped

### Token Amounts
- All amounts are in lamports/base units (not human-readable)
- 1 SOL = 1,000,000,000 lamports
- 1 USDC = 1,000,000 lamports (6 decimals)

### Slippage
- Specified in basis points (bps): 1 bps = 0.01%
- Example: 25 bps = 0.25%, 100 bps = 1%

### Native SOL Handling
- Native SOL mint: `So11111111111111111111111111111111111111112`
- `wrap_and_unwrap_sol = false`: wSOL stays in the wallet's wSOL ATA, which is never closed (the ATA
  must exist for a wSOL input). `true`: native SOL in/out; the wSOL ATA is closed afterwards.
- Titan has no input-side flag, so for `true` with a SOL input we prepend the wrap instructions
  and ask Titan to close the input account; `outputWsol` controls the output side.
- `swap*` doesn't create an ATA for a native-SOL output

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
    ├── dflow/
    │   └── mod.rs                  # DFlow HTTP client
    └── titan/
        ├── mod.rs                  # Titan venue (quote params, build)
        ├── client.rs               # Titan WebSocket client (routing, deadlines, reconnect)
        ├── types.rs                # Titan request/response types
        └── transaction_builder.rs # Titan instruction assembly and lookup tables

examples/
└── basic_swap.rs                   # Comprehensive usage examples

tests/
└── integration_swap_test.rs        # End-to-end swap test
```

## Dependencies

- **solana-sdk** / **solana-program** 3.0 and **solana-client** 3.1 (pinned in Cargo.toml)
- **jupiter-swap-api-client**: From GitHub (IliaZyrin fork, rev pinned in Cargo.toml)
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

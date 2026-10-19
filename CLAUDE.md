# CLAUDE.md

Library for best-price exact-in swaps across Jupiter, Titan and DFlow. See README.md for the API
and behaviour.

## Layout

```
src/
├── lib.rs
├── config.rs                       # ClientConfig, venue configs
├── client.rs                       # DexSuperAggClient: parallel quotes, build_best, min-out, swap_instructions, send_and_confirm
└── aggregators/
    ├── mod.rs                      # DexAggregator trait, Quote/QuotePayload, RouteQuote, misfit
    ├── jupiter/mod.rs              # Jupiter HTTP API (dynamicComputeUnitLimit, simulation errors rejected; maxAccounts + /swap-instructions for routes)
    ├── dflow/mod.rs                # DFlow HTTP API (quote JSON passed back to /swap unchanged; /swap-instructions for routes)
    └── titan/
        ├── mod.rs                  # Titan venue: quote params (outputWsol), build, route instructions
        ├── client.rs               # WebSocket client: id routing, deadlines, reconnect, route filter (+ mock-server tests)
        ├── types.rs                # MessagePack protocol types (only the fields we use)
        └── transaction_builder.rs  # compute budget + route instructions, lookup tables
tests/live.rs                       # ignored: build + simulate on real venues
```

## Rules

- A venue builds exactly the quote it returned; the client compares quotes, then builds.
- wSOL is never wrapped/unwrapped/closed (`wrapAndUnwrapSol: false`, Titan `outputWsol: true`,
  `closeInputTokenAccount: false`). Consumers keep SOL as wSOL in their ATA.
- Venues are peers: same trait, same parallel quote, same build. Which of them a consumer may
  run inside a liquidation transaction is the consumer's concern (configure only those).
- Routes and transactions are judged by guaranteed output (after slippage), never by expected
  output. `swap_instructions` returns the route alone (plus the ATA creations a multi-hop route
  needs): the caller owns the compute budget and its own token accounts, and the venue does not
  simulate (the input may only arrive in the caller's tx).

## Follow-ups

- Jupiter's `/swap/v1` (Metis) is no longer maintained upstream; the maintained equivalent is
  Swap v2 `/build` (Metis only, caller submits). v1 still works; migrate before it's switched off.

## Commands

```sh
cargo test
cargo clippy --all-targets
cargo test --test live -- --ignored --nocapture   # needs env vars, see tests/live.rs
```

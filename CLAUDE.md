# CLAUDE.md

Library for best-price exact-in swaps across Jupiter, Titan and DFlow. See README.md for the API
and behaviour.

## Layout

```
src/
├── lib.rs
├── config.rs                       # ClientConfig, venue configs
├── client.rs                       # DexSuperAggClient: parallel quotes, build_best, min-out, send_and_confirm
└── aggregators/
    ├── mod.rs                      # DexAggregator trait, Quote/QuotePayload
    ├── jupiter/mod.rs              # Jupiter HTTP API (dynamicComputeUnitLimit, simulation errors rejected)
    ├── dflow/mod.rs                # DFlow HTTP API (quote JSON passed back to /swap unchanged)
    └── titan/
        ├── mod.rs                  # Titan venue: quote params (outputWsol), build
        ├── client.rs               # WebSocket client: id routing, deadlines, reconnect (+ mock-server tests)
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

## Commands

```sh
cargo test
cargo clippy --all-targets
cargo test --test live -- --ignored --nocapture   # needs env vars, see tests/live.rs
```

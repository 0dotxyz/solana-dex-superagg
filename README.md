# solana-dex-superagg

Best-price exact-in swaps on Solana across the Jupiter, Titan and DFlow aggregators.

```rust
let client = DexSuperAggClient::new(
    ClientConfig {
        rpc_url,
        slippage_bps: 100,
        compute_unit_price_micro_lamports: 10_000,
        jupiter: Some(JupiterConfig { jup_swap_api_url, api_key }),
        titan: Some(TitanConfig { titan_ws_endpoint, titan_api_key }),
        dflow: Some(DflowConfig { api_url, api_key }),
        ..Default::default()
    },
    keypair,
)?;

// Buy at least `min_out`, spending `amount` and, if the quote falls short, at most `max_in`.
let swap = client.build_swap_transaction(usdc, wsol, amount, min_out, Some(max_in)).await?;
client.send_and_confirm(&swap.transaction, CommitmentLevel::Confirmed).await?;

// The same swap as instructions (plus the lookup tables they use) for a transaction of your own.
let swap = client.build_swap_instructions(usdc, wsol, amount, min_out, Some(max_in)).await?;
```

- **Routing**: every configured venue is quoted in parallel (`quote_timeout` each); failures,
  including rate limits, are logged and left out, so each extra venue is redundancy for free. The
  best quote is built (`build_timeout`), falling back to the next best if its venue fails to build.
- **Min-out**: `min_out` is checked against what the route guarantees at the configured slippage,
  not its expected output. A short quote raises the input by the shortfall and re-quotes (at most
  twice), never above `max_in` (default 1.10 × `amount`).
- **Instructions**: `build_swap_instructions` returns the venue's setup (output ATA) and swap
  instructions without a compute budget; the caller compiles, signs and submits. The venue does
  not simulate here, since the input may only arrive earlier in the caller's transaction.
- **wSOL** is an ordinary SPL token: swapped from/into the wallet's wSOL ATA (which must exist for a
  wSOL input), never wrapped, unwrapped or closed.
- **Titan**: one WebSocket shared by all callers, opened lazily; replies are routed by request and
  stream id, every request has a deadline, and a dropped or timed-out connection is replaced. Its
  transactions get a compute budget (`computeUnitsSafe` limit plus the configured CU price).
- Amounts are in base units; slippage in basis points.

## Adding a venue

Implement `DexAggregator` (`src/aggregators/mod.rs`): `quote` returns the expected and the
guaranteed (slippage-adjusted) output plus a `QuotePayload` variant holding whatever the venue
needs to build; `build_transaction` builds and signs exactly that quote, `build_instructions`
returns its instructions and lookup tables. Honour the wSOL rule above and the configured CU
price, add an `Aggregator` variant and a config struct, and push the venue in
`DexSuperAggClient::new`.

## Testing

```sh
cargo test
# Builds and simulates a 1 USDC → wSOL swap on each configured venue (nothing is sent):
RPC_URL=… WALLET='[1,2,…]' JUP_API_KEY=… TITAN_WS_ENDPOINT=… TITAN_API_KEY=… DFLOW_API_KEY=… \
  cargo test --test live -- --ignored --nocapture
```

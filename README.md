# solana-dex-superagg

Best-price exact-in swaps on Solana across the Jupiter and Titan aggregators, used by the eva01 and
zeeper liquidators.

```rust
let client = DexSuperAggClient::new(
    ClientConfig {
        rpc_url,
        slippage_bps: 100,
        compute_unit_price_micro_lamports: 10_000,
        jupiter: Some(JupiterConfig { jup_swap_api_url, api_key }),
        titan: Some(TitanConfig { titan_ws_endpoint, titan_api_key }),
        ..Default::default()
    },
    keypair,
)?;

// Buy at least `min_out`, spending `amount` and, if the quote falls short, at most `max_in`.
let swap = client.build_swap_transaction(usdc, wsol, amount, min_out, Some(max_in)).await?;
client.send_and_confirm(&swap.transaction, CommitmentLevel::Confirmed).await?;
```

- **Routing**: every configured venue is quoted in parallel (`quote_timeout` each); failures are
  logged and left out. The best quote is built (`build_timeout`), falling back to the next best if
  its venue fails to build.
- **Min-out**: with `min_out > 0`, a short quote raises the input by the shortfall and re-quotes (at
  most twice), never above `max_in` (default 1.10 × `amount`).
- **wSOL** is an ordinary SPL token: swapped from/into the wallet's wSOL ATA (which must exist for a
  wSOL input), never wrapped, unwrapped or closed.
- **Titan**: one WebSocket shared by all callers, opened lazily; replies are routed by request and
  stream id, every request has a deadline, and a dropped or timed-out connection is replaced. Its
  routes get a compute budget (`computeUnitsSafe` limit plus the configured CU price).
- Amounts are in base units; slippage in basis points.

## Adding a venue

Implement `DexAggregator` (`src/aggregators/mod.rs`): `quote` returns the expected output plus a
`QuotePayload` variant holding whatever the venue needs to build; `build_transaction` builds and
signs exactly that quote. Honour the wSOL rule above and the configured CU price, add an
`Aggregator` variant and a config struct, and push the venue in `DexSuperAggClient::new`.

## Testing

```sh
cargo test
# Builds and simulates a 1 USDC → wSOL swap on each configured venue (nothing is sent):
RPC_URL=… WALLET='[1,2,…]' JUP_API_KEY=… TITAN_WS_ENDPOINT=… TITAN_API_KEY=… \
  cargo test --test live -- --ignored --nocapture
```

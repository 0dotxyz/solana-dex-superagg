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

// Sell `amount`, guaranteed to receive at least `min_out`.
let swap = client.build_swap_transaction(usdc, wsol, amount, min_out).await?;
client.send_and_confirm(&swap.transaction, CommitmentLevel::Confirmed).await?;
```

- **Routing**: every configured venue is quoted in parallel (`quote_timeout` each); failures,
  including rate limits, are logged and left out, so each extra venue is redundancy for free. The
  quote with the most guaranteed output is built (`build_timeout`), falling back to the next if
  its venue fails to build.
- **Min-out**: `min_out` is checked against what the route guarantees at the configured slippage,
  not its expected output.
- **wSOL** is an ordinary SPL token: swapped from/into the wallet's wSOL ATA (which must exist for a
  wSOL input), never wrapped, unwrapped or closed.
- **Titan**: one WebSocket shared by all callers, opened lazily; replies are routed by request and
  stream id, every request has a deadline, and a dropped or timed-out connection is replaced. Its
  routes get a compute budget (`computeUnitsSafe` limit plus the configured CU price).
- Amounts are in base units; slippage in basis points.

## Instructions for your own transaction

`build_swap_instructions` hands a route out as instructions, for a caller that swaps inside its own
transaction, such as a marginfi order execution or liquidation:

```rust
// Routes for `user` paying at least `min_out` once slippage is counted, best first.
let routes = client
    .build_swap_instructions(&RouteRequest {
        user,
        input_mint: usdc,
        output_mint: wsol,
        amount: estimate,
        min_out,
        sizing: Sizing::ToMinOut { max_in },
        slippage_bps: 50,
        transaction_accounts,
        max_accounts: 30,
        allowed_programs: Some(vec![jupiter_program, titan_program, dflow_program, ata_program]),
    })
    .await?;
```

- **Guaranteed output**: routes are quoted at the request's `slippage_bps`, not the client's, and
  ranked and accepted by what they pay once it is counted (Jupiter's and DFlow's
  `otherAmountThreshold`, Titan's output less its `slippageBps`), not by their expected output.
- **Sizing**: with `Sizing::ToMinOut`, the input starts at `amount` and is resized after each
  quote (at most 4 quotes), never above `max_in`, toward the least whose best route covers
  `min_out` by at most `SIZING_TOLERANCE_BPS` (5). With `Sizing::Fixed`, every route swaps exactly
  `amount`. `RouteRequest::new` builds a fixed, unlimited request to adjust.
- **Limits**: a route may add at most `max_accounts` accounts beyond the `transaction_accounts` the
  caller's transaction already references, and its top-level instructions may call only
  `allowed_programs` (the programs those call in turn are not checked). Jupiter is quoted with
  `maxAccounts`; any route that breaks a limit is left out.
- **The route alone**: built for `user`, with no compute budget, wrapping or closing; a multi-hop
  route may create its intermediate token accounts (ATA program, so allow it). The user's token
  accounts for both mints must exist. The venue does not simulate here, since the input may only
  arrive earlier in the caller's transaction. The lookup tables are optional; a transaction may
  list the accounts inline instead.
- Every route that covers `min_out` is returned, so a caller can fall back to the next.

## Adding a venue

Implement `DexAggregator` (`src/aggregators/mod.rs`): `quote` returns the guaranteed
(slippage-adjusted) output plus a `QuotePayload` variant holding whatever the venue needs to
build; `build_transaction` builds and signs exactly that quote. Honour the wSOL rule
above and the configured CU price, add an `Aggregator` variant and a config struct, and push the
venue in `DexSuperAggClient::new`. `quote` quotes at the request's slippage and
`build_instructions` returns that quote's route; the client assembles the `SwapInstructions`.

## Testing

```sh
# Unit tests need no network: the client is exercised against mock venues and a mock RPC, the
# Titan client against a local WebSocket server, and the HTTP venues' parsing against fixtures.
cargo test
# Builds and simulates a 1 USDC → wSOL swap on each configured venue, as a transaction and as
# sized and fixed instructions (nothing is sent):
RPC_URL=… WALLET='[1,2,…]' JUP_API_KEY=… TITAN_WS_ENDPOINT=… TITAN_API_KEY=… DFLOW_API_KEY=… \
  cargo test --test live -- --ignored --nocapture
```

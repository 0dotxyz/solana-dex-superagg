# Aggregator Integration Guide

How to add a new DEX aggregator (Raydium, Orca, OKX, …) to `solana-dex-superagg`.

## How a venue fits in

Every venue implements the two-phase `DexAggregator` trait (`src/aggregators/mod.rs`):

```rust
#[async_trait]
pub trait DexAggregator: Send + Sync {
    fn kind(&self) -> Aggregator;
    async fn quote(&self, request: &SwapRequest) -> Result<Quote>;
    async fn build_transaction(&self, request: &SwapRequest, quote: &Quote)
        -> Result<VersionedTransaction>;
}
```

`DexSuperAggClient` (`src/client.rs`) does the rest, for every venue alike:

- quotes all configured venues **in parallel**, each under `shared.quote_timeout_ms`;
- logs and drops venues that fail or time out, sorts the rest by `out_amount`;
- builds the best quote under `shared.build_timeout_ms`, falling back to the next best if a build
  fails;
- for min-out swaps, raises the input (capped) and re-quotes;
- submits and confirms (`swap*`), or hands back the signed transaction (`build_swap_transaction*`).

So a venue only has to answer two questions: *what would this swap yield?* and *give me a signed
transaction for exactly that quote*. It must not retry, sleep, submit, or swallow errors itself.

### Rules every venue must follow

1. **Build what you quoted.** `build_transaction` receives the `Quote` your `quote` returned. Build
   from its payload; don't re-quote (the client compared venues on that number).
2. **Honour `SwapRequest::wrap_and_unwrap_sol` exactly** (see its doc comment). `false` means a wSOL
   input/output stays in the wallet's wSOL ATA and the ATA is never closed; `true` means native SOL
   in and out. If the venue's API has a flag for this, map it; if it returns raw instructions, use
   the helpers in `src/aggregators/sol.rs` (see Titan).
3. **Apply `compute_unit_price_micro_lamports`** as a per-CU price (not a total fee). If the venue
   returns raw instructions, also set a CU limit (`sol::set_compute_unit_limit`) — without one the
   transaction runs under the 200k default.
4. **Reuse connections.** Create HTTP/WebSocket clients once in the constructor. The client
   applies timeouts around your calls, but long-lived sockets need their own reconnect logic (see
   `src/aggregators/titan/client.rs`).
5. **Return errors, don't log-and-continue.** The client logs per-venue failures with context.

## Step 1: Implement the venue

Create `src/aggregators/your_aggregator/mod.rs`:

```rust
use crate::aggregators::{
    DexAggregator, Quote, QuoteMetadata, QuotePayload, QuoteResult, SwapRequest,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use solana_sdk::{signature::Keypair, transaction::VersionedTransaction};
use std::{sync::Arc, time::Instant};

/// Whatever the venue's quote endpoint returns; kept to build from later.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct YourQuote { /* ... */ }

pub struct YourAggregator {
    http_client: reqwest::Client,
    signer: Arc<Keypair>,
    api_url: String,
}

impl YourAggregator {
    pub fn new(config: &ClientConfig, signer: Arc<Keypair>) -> Result<Self> {
        let your_config = config
            .your_aggregator
            .as_ref()
            .ok_or_else(|| anyhow!("YourAggregator configuration not found"))?;
        Ok(Self {
            http_client: reqwest::Client::new(),
            signer,
            api_url: your_config.api_url.clone(),
        })
    }
}

#[async_trait::async_trait]
impl DexAggregator for YourAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::YourAggregator
    }

    async fn quote(&self, request: &SwapRequest) -> Result<Quote> {
        let start = Instant::now();
        let quote: YourQuote = /* GET {api_url}/quote with request.input_mint, output_mint,
                                  amount, slippage_bps */ todo!();
        let out_amount = /* expected (not minimum) output */ todo!();
        Ok(Quote {
            aggregator: Aggregator::YourAggregator,
            out_amount,
            result: QuoteResult {
                out_amount,
                price_impact: 0.0,
                metadata: QuoteMetadata::default(),
                quote_time: Some(start.elapsed()),
            },
            payload: QuotePayload::YourAggregator(Box::new(quote)),
        })
    }

    async fn build_transaction(
        &self,
        request: &SwapRequest,
        quote: &Quote,
    ) -> Result<VersionedTransaction> {
        let QuotePayload::YourAggregator(your_quote) = &quote.payload else {
            return Err(anyhow!("YourAggregator cannot build a {:?} quote", quote.aggregator));
        };
        // POST the quote back with request.wrap_and_unwrap_sol and
        // request.compute_unit_price_micro_lamports, deserialize the transaction, then sign:
        let unsigned: VersionedTransaction = todo!();
        VersionedTransaction::try_new(unsigned.message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))
    }
}
```

`out_amount` must mean the same thing on every venue — the **expected** output, not the
slippage-adjusted minimum — or the best-price comparison is biased.

## Step 2: Register the quote payload and module

In `src/aggregators/mod.rs`:

```rust
pub mod your_aggregator;
pub use your_aggregator::YourAggregator;

pub(crate) enum QuotePayload {
    // ...
    YourAggregator(Box<your_aggregator::YourQuote>),
}
```

## Step 3: Add configuration

In `src/config.rs`:

1. Add `YourAggregator` to the `Aggregator` enum.
2. Add a `YourAggregatorConfig` struct (`#[serde(default)]`, with a `Default` impl) and an
   `pub your_aggregator: Option<YourAggregatorConfig>` field on `ClientConfig`
   (`#[serde(default)]`). It loads from `DEX_SUPERAGG_YOUR_AGGREGATOR__*`.
3. Add `is_your_aggregator_configured()`.
4. Add a `validate_your_aggregator()` connectivity check and call it from `validate()` for
   `BestPrice`, `LowestSlippageClimber`, and `PreferredAggregator { YourAggregator }`.

## Step 4: Wire it into the client

In `src/client.rs`:

1. Add `your_aggregator: Option<YourAggregator>` to `DexSuperAggClient` and construct it in `new()`
   when `config.is_your_aggregator_configured()`, like the existing venues.
2. Add the arm to `venue()`:
   ```rust
   Aggregator::YourAggregator => self.your_aggregator.as_ref().map(|v| v as &dyn DexAggregator),
   ```
3. Add `Aggregator::YourAggregator` to the list in `venues()`. Its position breaks exact ties in
   output amount.
4. If the venue holds a connection, close it in `close()`.

That's all — quoting, comparison, fallback, min-out handling and submission come for free.

## Step 5: Test

- Unit-test the request/response mapping and, especially, the SOL flags: for both values of
  `wrap_and_unwrap_sol`, check what the venue is asked to do with a wSOL input and a wSOL output
  (see the tests in `src/aggregators/titan/mod.rs`).
- If the venue returns raw instructions, test the assembled instruction list (compute budget, wrap,
  route) like `src/aggregators/titan/transaction_builder.rs`.
- A WebSocket venue should get mock-server tests for concurrent requests, an unanswered request
  (must time out, not hang), and a server-side disconnect (must reconnect) — see
  `src/aggregators/titan/client.rs`.
- Run the ignored end-to-end test with real credentials:
  `cargo test test_all_swap -- --nocapture --ignored`.

## Checklist

- [ ] `src/aggregators/your_aggregator/mod.rs` implements `kind`, `quote`, `build_transaction`
- [ ] `quote` returns the expected output; `build_transaction` builds that exact quote
- [ ] `wrap_and_unwrap_sol` and the CU price are honoured
- [ ] HTTP/WebSocket clients are created once
- [ ] `QuotePayload` variant and module export added
- [ ] `Aggregator` variant, config struct, `is_*_configured`, validation added
- [ ] Client field, `new()`, `venue()`, `venues()` and (if needed) `close()` updated
- [ ] Unit tests for request mapping and SOL handling; mock tests for sockets
- [ ] README environment variables documented

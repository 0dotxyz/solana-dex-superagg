use crate::aggregators::{
    dflow::DflowAggregator, jupiter::JupiterAggregator, misfit, titan::TitanAggregator,
    DexAggregator, Quote,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, bail, Result};
use futures_util::future::join_all;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{timeout, Instant};

const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Rebroadcast the pending transaction every this many polls.
const REBROADCAST_EVERY_POLLS: u32 = 4;

/// Maximum quotes `build_swap_instructions` takes while sizing the input.
const SIZING_QUOTES: usize = 4;

/// Sizing stops once the guaranteed output covers `min_out` without overshooting it by more than
/// this much.
pub const SIZING_TOLERANCE_BPS: u64 = 5;

/// One venue's quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueQuote {
    pub aggregator: Aggregator,
    /// Output the venue guarantees after slippage.
    pub min_out_amount: u64,
}

/// A signed swap transaction, not yet submitted.
#[derive(Debug, Clone)]
pub struct SwapTransaction {
    pub transaction: VersionedTransaction,
    pub in_amount: u64,
    /// Output the venue guarantees after slippage; at least the requested `min_out`.
    pub min_out_amount: u64,
    pub aggregator: Aggregator,
}

/// A swap to embed in the caller's own transaction.
#[derive(Debug, Clone)]
pub struct RouteRequest {
    /// Wallet that owns the token accounts and signs the caller's transaction.
    pub user: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// Input amount. With `Sizing::ToMinOut` it is only the starting point.
    pub amount: u64,
    /// Minimum output the route must guarantee after slippage.
    pub min_out: u64,
    pub sizing: Sizing,
    /// Slippage to quote at.
    pub slippage_bps: u16,
    /// Accounts the caller's transaction already contains. They don't count against
    /// `max_accounts`.
    pub transaction_accounts: Vec<Pubkey>,
    /// Maximum number of new accounts (programs included) the route may add to the transaction.
    pub max_accounts: usize,
    /// Programs the route's top-level instructions may call. `None` allows any.
    pub allowed_programs: Option<Vec<Pubkey>>,
}

impl RouteRequest {
    /// A request that swaps exactly `amount`, allows any program and up to 64 new accounts.
    /// Override fields as needed.
    pub fn new(
        user: Pubkey,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        min_out: u64,
        slippage_bps: u16,
    ) -> Self {
        Self {
            user,
            input_mint,
            output_mint,
            amount,
            min_out,
            sizing: Sizing::Fixed,
            slippage_bps,
            transaction_accounts: vec![],
            max_accounts: 64,
            allowed_programs: None,
        }
    }
}

/// How `build_swap_instructions` chooses the input amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    /// Swap exactly `amount`.
    Fixed,
    /// Start from `amount` and resize it, up or down but never above `max_in`, until the
    /// guaranteed output just covers `min_out`.
    ToMinOut { max_in: u64 },
}

/// A route as instructions, for the requesting wallet's own transaction.
#[derive(Debug, Clone)]
pub struct SwapInstructions {
    pub aggregator: Aggregator,
    pub in_amount: u64,
    /// Output the venue guarantees after slippage. The route fails on-chain below it.
    pub min_out_amount: u64,
    /// The swap instructions, plus any token accounts the route creates for itself (ATA program).
    /// No compute budget and no wrapping or unwrapping. The wallet's token accounts for both mints
    /// must already exist.
    pub instructions: Vec<Instruction>,
    /// Lookup tables that cover the route's accounts.
    pub address_lookup_tables: Vec<Pubkey>,
}

/// Routes exact-in swaps across DEX aggregators (Jupiter, Titan, DFlow).
pub struct DexSuperAggClient {
    rpc_client: Arc<RpcClient>,
    config: ClientConfig,
    /// The signer, which whole-transaction swaps are quoted and built for.
    user: Pubkey,
    /// Configured venues; their order breaks ties in output amount.
    venues: Vec<Box<dyn DexAggregator>>,
}

/// The next input to try: `amount` scaled by `target / got`, where `got` is what `amount`
/// guaranteed and `target` is `min_out` plus half the tolerance. Never above `max_in`.
fn sized_input(amount: u64, got: u64, min_out: u64, max_in: u64) -> u64 {
    let target =
        (u128::from(min_out) * (20_000 + u128::from(SIZING_TOLERANCE_BPS))).div_ceil(20_000);
    let sized = (u128::from(amount) * target).div_ceil(u128::from(got.max(1)));
    u64::try_from(sized).unwrap_or(u64::MAX).min(max_in)
}

/// Whether `got` covers `min_out` without exceeding it by more than `SIZING_TOLERANCE_BPS`.
fn within_tolerance(got: u64, min_out: u64) -> bool {
    min_out == 0
        || (got >= min_out
            && u128::from(got) * 10_000
                <= u128::from(min_out) * (10_000 + u128::from(SIZING_TOLERANCE_BPS)))
}

/// `future`'s result, or an error once `limit` passes.
async fn within<T>(limit: Duration, future: impl Future<Output = Result<T>>) -> Result<T> {
    match timeout(limit, future).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("timed out after {:?}", limit)),
    }
}

fn check_swap(input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<()> {
    if input_mint == output_mint {
        bail!("Input and output mints cannot be the same");
    }
    if amount == 0 {
        bail!("Swap amount cannot be zero");
    }
    Ok(())
}

/// The successful results. Each failure is logged with `failed` as the message. When nothing
/// succeeded, fails with `none` and every venue's error.
fn successes<T>(results: Vec<(Aggregator, Result<T>)>, failed: &str, none: &str) -> Result<Vec<T>> {
    let mut successes = Vec::new();
    let mut failures = Vec::new();
    for (aggregator, result) in results {
        match result {
            Ok(success) => successes.push(success),
            Err(e) => {
                tracing::warn!(aggregator = ?aggregator, error = %e, "{}", failed);
                failures.push(format!("{:?}: {}", aggregator, e));
            }
        }
    }
    if successes.is_empty() {
        bail!("{} ({})", none, failures.join("; "));
    }
    Ok(successes)
}

impl DexSuperAggClient {
    pub fn new(config: ClientConfig, signer: Keypair) -> Result<Self> {
        let signer = Arc::new(signer);
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let mut venues: Vec<Box<dyn DexAggregator>> = Vec::new();
        if let Some(jupiter) = &config.jupiter {
            venues.push(Box::new(JupiterAggregator::new(
                jupiter,
                &config,
                Arc::clone(&signer),
            )?));
        }
        if let Some(titan) = &config.titan {
            venues.push(Box::new(TitanAggregator::new(
                titan,
                &config,
                Arc::clone(&signer),
                Arc::clone(&rpc_client),
            )));
        }
        if let Some(dflow) = &config.dflow {
            venues.push(Box::new(DflowAggregator::new(
                dflow,
                &config,
                Arc::clone(&signer),
            )));
        }
        if venues.is_empty() {
            bail!("No DEX aggregator is configured");
        }
        Ok(Self::with_venues(
            rpc_client,
            config,
            signer.pubkey(),
            venues,
        ))
    }

    fn with_venues(
        rpc_client: Arc<RpcClient>,
        config: ClientConfig,
        user: Pubkey,
        venues: Vec<Box<dyn DexAggregator>>,
    ) -> Self {
        Self {
            rpc_client,
            config,
            user,
            venues,
        }
    }

    fn venue(&self, aggregator: Aggregator) -> Result<&dyn DexAggregator> {
        self.venues
            .iter()
            .find(|venue| venue.kind() == aggregator)
            .map(|venue| venue.as_ref())
            .ok_or_else(|| anyhow!("{:?} is not configured", aggregator))
    }

    /// Runs `quote` on every venue in parallel, each under `quote_timeout`. Venues that fail are
    /// logged and skipped; it is an error only when all of them fail.
    async fn quote_each<'a, T, Fut>(
        &self,
        venues: impl Iterator<Item = &'a dyn DexAggregator>,
        quote: impl Fn(&'a dyn DexAggregator) -> Fut,
    ) -> Result<Vec<T>>
    where
        Fut: Future<Output = Result<T>>,
    {
        let quote_timeout = self.config.quote_timeout;
        let results = join_all(venues.map(|venue| {
            let quoted = quote(venue);
            async move { (venue.kind(), within(quote_timeout, quoted).await) }
        }))
        .await;

        successes(results, "Quote failed", "No aggregator returned a quote")
    }

    /// Quotes every venue for `request` at `amount`, sorted by guaranteed output, best first.
    async fn collect_quotes(&self, request: &RouteRequest, amount: u64) -> Result<Vec<Quote>> {
        check_swap(&request.input_mint, &request.output_mint, amount)?;
        let mut quotes = self
            .quote_each(self.venues.iter().map(|venue| venue.as_ref()), |venue| {
                venue.quote(request, amount)
            })
            .await?;

        // Stable: on a tie the configured order decides.
        quotes.sort_by(|a, b| b.min_out_amount.cmp(&a.min_out_amount));
        Ok(quotes)
    }

    /// A request for the whole-transaction path: the signer swaps at the configured slippage, and
    /// the route may use any number of accounts (the transaction has lookup tables for them).
    fn transaction_request(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
    ) -> RouteRequest {
        RouteRequest {
            max_accounts: usize::MAX,
            ..RouteRequest::new(
                self.user,
                input_mint,
                output_mint,
                amount,
                0,
                self.config.slippage_bps,
            )
        }
    }

    /// Builds the instructions of every quote that guarantees `min_out`, in parallel, keeping the
    /// order. A quote whose venue fails to build, or whose route breaks the request's limits, is
    /// logged and dropped.
    async fn build_routes(
        &self,
        request: &RouteRequest,
        amount: u64,
        quotes: &[Quote],
    ) -> Result<Vec<SwapInstructions>> {
        let build_timeout = self.config.build_timeout;
        let results = join_all(
            quotes
                .iter()
                .filter(|quote| quote.min_out_amount >= request.min_out)
                .map(|quote| async move {
                    let built = async {
                        let venue = self.venue(quote.aggregator)?;
                        let route =
                            within(build_timeout, venue.build_instructions(request, quote)).await?;
                        if let Some(reason) = misfit(&route.instructions, request) {
                            bail!("its route {}", reason);
                        }
                        Ok(SwapInstructions {
                            aggregator: quote.aggregator,
                            in_amount: amount,
                            min_out_amount: quote.min_out_amount,
                            instructions: route.instructions,
                            address_lookup_tables: route.address_lookup_tables,
                        })
                    }
                    .await;
                    (quote.aggregator, built)
                }),
        )
        .await;
        successes(
            results,
            "Failed to build swap instructions",
            "Could not build swap instructions on any venue",
        )
    }

    /// Builds the best quote that guarantees `min_out`. If its venue fails to build, tries the
    /// next one.
    async fn build_best(
        &self,
        quotes: &[Quote],
        amount: u64,
        min_out: u64,
    ) -> Result<SwapTransaction> {
        let summary: Vec<_> = quotes
            .iter()
            .map(|q| (q.aggregator, q.min_out_amount))
            .collect();
        let mut failures = Vec::new();
        for quote in quotes
            .iter()
            .filter(|quote| quote.min_out_amount >= min_out)
        {
            let venue = self.venue(quote.aggregator)?;
            match within(self.config.build_timeout, venue.build_transaction(quote)).await {
                Ok(transaction) => {
                    tracing::info!(
                        aggregator = ?quote.aggregator,
                        in_amount = amount,
                        min_out_amount = quote.min_out_amount,
                        quotes = ?summary,
                        "Selected aggregator"
                    );
                    return Ok(SwapTransaction {
                        transaction,
                        in_amount: amount,
                        min_out_amount: quote.min_out_amount,
                        aggregator: quote.aggregator,
                    });
                }
                Err(e) => {
                    tracing::warn!(aggregator = ?quote.aggregator, error = %e, "Failed to build swap transaction");
                    failures.push(format!("{:?}: {}", quote.aggregator, e));
                }
            }
        }
        bail!(
            "Could not build a swap transaction on any venue ({})",
            failures.join("; ")
        )
    }

    /// Every venue's quote for swapping `amount`, best guaranteed output first.
    pub async fn quote(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
    ) -> Result<Vec<VenueQuote>> {
        Ok(self
            .collect_quotes(
                &self.transaction_request(input_mint, output_mint, amount),
                amount,
            )
            .await?
            .into_iter()
            .map(|quote| VenueQuote {
                aggregator: quote.aggregator,
                min_out_amount: quote.min_out_amount,
            })
            .collect())
    }

    /// Builds a signed swap of `amount` that guarantees at least `min_out` after slippage (0 for
    /// no minimum).
    pub async fn build_swap_transaction(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        min_out: u64,
    ) -> Result<SwapTransaction> {
        let request = self.transaction_request(input_mint, output_mint, amount);
        let quotes = self.collect_quotes(&request, amount).await?;
        let best_out = quotes[0].min_out_amount;
        if best_out < min_out {
            bail!(
                "Best quote for {} {} guarantees {} {}, short of the {} needed",
                amount,
                input_mint,
                best_out,
                output_mint,
                min_out
            );
        }
        self.build_best(&quotes, amount, min_out).await
    }

    /// Routes for `request` as instructions, every one guaranteeing at least `request.min_out`,
    /// best first. The input is `request.amount`, or the sized amount with `Sizing::ToMinOut`.
    pub async fn build_swap_instructions(
        &self,
        request: &RouteRequest,
    ) -> Result<Vec<SwapInstructions>> {
        let (covering, max_in) = match request.sizing {
            Sizing::Fixed => {
                let quotes = self.collect_quotes(request, request.amount).await?;
                let covers = quotes[0].min_out_amount >= request.min_out;
                (covers.then_some((request.amount, quotes)), request.amount)
            }
            Sizing::ToMinOut { max_in } => (self.sized_quotes(request, max_in).await?, max_in),
        };

        let (amount, quotes) = covering.ok_or_else(|| {
            anyhow!(
                "No route pays {} {} for at most {} {}",
                request.min_out,
                request.output_mint,
                max_in,
                request.input_mint
            )
        })?;
        self.build_routes(request, amount, &quotes).await
    }

    /// Finds the smallest input, starting from `request.amount` and never above `max_in`, whose
    /// best quote guarantees `min_out`, stopping once it does so within `SIZING_TOLERANCE_BPS`.
    /// Returns that input with its quotes, or `None` if no input covers `min_out`.
    async fn sized_quotes(
        &self,
        request: &RouteRequest,
        max_in: u64,
    ) -> Result<Option<(u64, Vec<Quote>)>> {
        let min_out = request.min_out;
        if request.amount > max_in {
            bail!(
                "input {} already exceeds the input cap {}",
                request.amount,
                max_in
            );
        }

        let mut amount = request.amount;
        let mut least_covering: Option<(u64, Vec<Quote>)> = None;
        for _ in 0..SIZING_QUOTES {
            let quotes = match self.collect_quotes(request, amount).await {
                Ok(quotes) => quotes,
                Err(e) if least_covering.is_some() => {
                    tracing::warn!(error = %e, "Sizing quote failed; keeping the covering one");
                    break;
                }
                Err(e) => return Err(e),
            };
            let best = quotes[0].min_out_amount;
            if best >= min_out {
                let tight = within_tolerance(best, min_out);
                if least_covering
                    .as_ref()
                    .is_none_or(|(covering, _)| amount < *covering)
                {
                    least_covering = Some((amount, quotes));
                }
                if tight {
                    break;
                }
            }
            let next = sized_input(amount, best, min_out, max_in);
            if next == amount {
                break;
            }
            tracing::debug!(
                from = amount,
                to = next,
                best,
                min_out,
                max_in,
                "Resizing input"
            );
            amount = next;
        }
        Ok(least_covering)
    }

    /// Submit `transaction` and wait until it reaches `commitment`, rebroadcasting while its
    /// blockhash is alive. Fails if the transaction errors, expires without landing, or doesn't
    /// confirm within `confirm_timeout`.
    pub async fn send_and_confirm(
        &self,
        transaction: &VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> Result<Signature> {
        let signature = self
            .rpc_client
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

        let commitment = CommitmentConfig { commitment };
        let blockhash = *transaction.message.recent_blockhash();
        let deadline = Instant::now() + self.config.confirm_timeout;
        let mut polls = 0u32;
        loop {
            tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
            polls += 1;

            match self
                .rpc_client
                .get_signature_status_with_commitment(&signature, commitment)
                .await
            {
                Ok(Some(Ok(()))) => return Ok(signature),
                Ok(Some(Err(e))) => bail!("Swap transaction {} failed: {}", signature, e),
                Ok(None) => {}
                Err(e) => tracing::debug!(%signature, error = %e, "Signature status poll failed"),
            }
            if Instant::now() >= deadline {
                bail!(
                    "Timed out waiting for swap transaction {} to confirm",
                    signature
                );
            }

            if polls.is_multiple_of(REBROADCAST_EVERY_POLLS) {
                let landed = self
                    .rpc_client
                    .get_signature_status_with_commitment(&signature, CommitmentConfig::processed())
                    .await
                    .ok()
                    .flatten();
                match landed {
                    Some(Err(e)) => bail!("Swap transaction {} failed: {}", signature, e),
                    // Landed; just waiting for the commitment level.
                    Some(Ok(())) => {}
                    None => {
                        let valid = self
                            .rpc_client
                            .is_blockhash_valid(&blockhash, CommitmentConfig::processed())
                            .await
                            .unwrap_or(true);
                        if !valid {
                            bail!("Swap transaction {} expired without landing", signature);
                        }
                        let _ = self
                            .rpc_client
                            .send_transaction_with_config(
                                transaction,
                                RpcSendTransactionConfig {
                                    skip_preflight: true,
                                    ..Default::default()
                                },
                            )
                            .await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregators::{QuotePayload, Route};
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use solana_client::rpc_request::RpcRequest;
    use solana_instruction::AccountMeta;
    use solana_rpc_client::mock_sender::MocksMap;
    use std::sync::Mutex;

    #[test]
    fn sizing_aims_at_half_the_tolerance_above_min_out() {
        // Short by 1%: scale up toward 1,000,250 guaranteed.
        assert_eq!(
            sized_input(1_000_000, 990_000, 1_000_000, u64::MAX),
            1_010_354
        );
        // Over by 1%: scale down toward it.
        assert_eq!(
            sized_input(1_000_000, 1_010_000, 1_000_000, u64::MAX),
            990_347
        );
    }

    #[test]
    fn sizing_never_exceeds_the_cap() {
        assert_eq!(
            sized_input(1_000_000, 500_000, 1_000_000, 1_500_000),
            1_500_000
        );
        assert_eq!(sized_input(1_000, 0, 1_000, 5_000), 5_000);
    }

    #[test]
    fn a_tight_route_covers_min_out_within_the_tolerance() {
        assert!(within_tolerance(1_000_000, 1_000_000));
        assert!(within_tolerance(1, 0));
        assert!(within_tolerance(1_000_500, 1_000_000));
        assert!(!within_tolerance(1_000_501, 1_000_000));
        assert!(!within_tolerance(999_999, 1_000_000));
    }

    #[test]
    fn a_swap_needs_two_mints_and_an_amount() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        assert!(check_swap(&a, &b, 1).is_ok());
        assert!(check_swap(&a, &a, 1).is_err());
        assert!(check_swap(&a, &b, 0).is_err());
    }

    const PROGRAM: Pubkey = Pubkey::new_from_array([7; 32]);

    /// A venue that guarantees `prices[n] / 10_000` of the input on its n-th quote (the last price
    /// repeats), or fails as told. Its route is one instruction of `program` with `accounts`
    /// accounts.
    struct MockVenue {
        kind: Aggregator,
        prices: Vec<u64>,
        quote_error: Option<&'static str>,
        quote_delay: Duration,
        build_error: Option<&'static str>,
        build_delay: Duration,
        program: Pubkey,
        accounts: usize,
        quoted: Arc<Mutex<Vec<u64>>>,
    }

    impl MockVenue {
        fn new(kind: Aggregator, price_bps: u64) -> Self {
            Self {
                kind,
                prices: vec![price_bps],
                quote_error: None,
                quote_delay: Duration::ZERO,
                build_error: None,
                build_delay: Duration::ZERO,
                program: PROGRAM,
                accounts: 2,
                quoted: Arc::default(),
            }
        }

        async fn build(&self) -> Result<()> {
            tokio::time::sleep(self.build_delay).await;
            match self.build_error {
                Some(error) => bail!(error),
                None => Ok(()),
            }
        }
    }

    #[async_trait]
    impl DexAggregator for MockVenue {
        fn kind(&self) -> Aggregator {
            self.kind
        }

        async fn quote(&self, _request: &RouteRequest, amount: u64) -> Result<Quote> {
            let price = {
                let mut quoted = self.quoted.lock().unwrap();
                let price = self.prices[quoted.len().min(self.prices.len() - 1)];
                quoted.push(amount);
                price
            };
            tokio::time::sleep(self.quote_delay).await;
            if let Some(error) = self.quote_error {
                bail!(error);
            }
            Ok(Quote {
                aggregator: self.kind,
                min_out_amount: amount * price / 10_000,
                payload: QuotePayload::Dflow(Box::new(Value::Null)),
            })
        }

        async fn build_transaction(&self, _quote: &Quote) -> Result<VersionedTransaction> {
            self.build().await?;
            Ok(VersionedTransaction::default())
        }

        async fn build_instructions(
            &self,
            _request: &RouteRequest,
            _quote: &Quote,
        ) -> Result<Route> {
            self.build().await?;
            Ok(Route {
                instructions: vec![Instruction {
                    program_id: self.program,
                    accounts: (0..self.accounts)
                        .map(|_| AccountMeta::new(Pubkey::new_unique(), false))
                        .collect(),
                    data: vec![],
                }],
                address_lookup_tables: vec![],
            })
        }
    }

    fn client(venues: Vec<MockVenue>) -> DexSuperAggClient {
        DexSuperAggClient::with_venues(
            Arc::new(RpcClient::new_mock("succeeds".to_string())),
            ClientConfig {
                quote_timeout: Duration::from_millis(100),
                build_timeout: Duration::from_millis(100),
                ..Default::default()
            },
            Pubkey::new_unique(),
            venues
                .into_iter()
                .map(|venue| Box::new(venue) as Box<dyn DexAggregator>)
                .collect(),
        )
    }

    fn route_request(amount: u64, min_out: u64, sizing: Sizing) -> RouteRequest {
        RouteRequest {
            sizing,
            allowed_programs: Some(vec![PROGRAM]),
            ..RouteRequest::new(
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                amount,
                min_out,
                50,
            )
        }
    }

    const USDC: Pubkey = Pubkey::new_from_array([1; 32]);
    const WSOL: Pubkey = Pubkey::new_from_array([2; 32]);

    #[tokio::test]
    async fn quotes_rank_by_guaranteed_output_and_configured_order_breaks_ties() {
        let client = client(vec![
            MockVenue::new(Aggregator::Jupiter, 9_800),
            MockVenue::new(Aggregator::Titan, 9_900),
            MockVenue::new(Aggregator::Dflow, 9_900),
        ]);
        let quotes = client.quote(USDC, WSOL, 1_000_000).await.unwrap();
        assert_eq!(
            quotes,
            [
                (Aggregator::Titan, 990_000),
                (Aggregator::Dflow, 990_000),
                (Aggregator::Jupiter, 980_000),
            ]
            .map(|(aggregator, min_out_amount)| VenueQuote {
                aggregator,
                min_out_amount
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_venue_that_fails_or_misses_the_deadline_is_left_out() {
        let slow = MockVenue {
            quote_delay: Duration::from_secs(1),
            ..MockVenue::new(Aggregator::Jupiter, 9_900)
        };
        let broken = MockVenue {
            quote_error: Some("rate limited"),
            ..MockVenue::new(Aggregator::Titan, 9_900)
        };
        let client = client(vec![slow, broken, MockVenue::new(Aggregator::Dflow, 9_000)]);
        let quotes = client.quote(USDC, WSOL, 1_000_000).await.unwrap();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].aggregator, Aggregator::Dflow);
    }

    #[tokio::test(start_paused = true)]
    async fn no_quote_at_all_reports_every_venue() {
        let slow = MockVenue {
            quote_delay: Duration::from_secs(1),
            ..MockVenue::new(Aggregator::Jupiter, 9_900)
        };
        let broken = MockVenue {
            quote_error: Some("rate limited"),
            ..MockVenue::new(Aggregator::Titan, 9_900)
        };
        let error = client(vec![slow, broken])
            .quote(USDC, WSOL, 1_000_000)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Jupiter: timed out"), "{}", error);
        assert!(error.contains("Titan: rate limited"), "{}", error);
    }

    #[tokio::test]
    async fn a_transaction_is_built_on_the_best_venue_that_can_build() {
        let best_but_broken = MockVenue {
            build_error: Some("simulation failed"),
            ..MockVenue::new(Aggregator::Jupiter, 9_900)
        };
        let client = client(vec![
            best_but_broken,
            MockVenue::new(Aggregator::Titan, 9_800),
        ]);
        let swap = client
            .build_swap_transaction(USDC, WSOL, 1_000_000, 970_000)
            .await
            .unwrap();
        assert_eq!(
            (swap.aggregator, swap.in_amount, swap.min_out_amount),
            (Aggregator::Titan, 1_000_000, 980_000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_venue_that_misses_the_build_deadline_is_passed_over() {
        let slow = MockVenue {
            build_delay: Duration::from_secs(1),
            ..MockVenue::new(Aggregator::Jupiter, 9_900)
        };
        let client = client(vec![slow, MockVenue::new(Aggregator::Titan, 9_800)]);
        let swap = client
            .build_swap_transaction(USDC, WSOL, 1_000_000, 0)
            .await
            .unwrap();
        assert_eq!(swap.aggregator, Aggregator::Titan);
    }

    #[tokio::test]
    async fn a_transaction_short_of_min_out_is_refused_before_building() {
        let client = client(vec![MockVenue::new(Aggregator::Jupiter, 9_900)]);
        let error = client
            .build_swap_transaction(USDC, WSOL, 1_000_000, 995_000)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("guarantees 990000"), "{}", error);
        assert!(error.contains("short of the 995000"), "{}", error);
    }

    #[tokio::test]
    async fn fixed_sizing_returns_every_covering_route_best_first() {
        let client = client(vec![
            MockVenue::new(Aggregator::Jupiter, 9_700),
            MockVenue::new(Aggregator::Titan, 9_900),
            MockVenue::new(Aggregator::Dflow, 9_800),
        ]);
        let routes = client
            .build_swap_instructions(&route_request(1_000_000, 980_000, Sizing::Fixed))
            .await
            .unwrap();
        let summary: Vec<_> = routes
            .iter()
            .map(|route| (route.aggregator, route.in_amount, route.min_out_amount))
            .collect();
        assert_eq!(
            summary,
            [
                (Aggregator::Titan, 1_000_000, 990_000),
                (Aggregator::Dflow, 1_000_000, 980_000)
            ]
        );
        assert_eq!(routes[0].instructions.len(), 1);
    }

    #[tokio::test]
    async fn fixed_sizing_fails_when_no_route_covers_min_out() {
        let client = client(vec![MockVenue::new(Aggregator::Jupiter, 9_700)]);
        let error = client
            .build_swap_instructions(&route_request(1_000_000, 980_000, Sizing::Fixed))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("No route pays 980000"), "{}", error);
        assert!(error.contains("for at most 1000000"), "{}", error);
    }

    #[tokio::test]
    async fn a_route_that_breaks_the_limits_is_dropped_after_building() {
        let other_program = MockVenue {
            program: Pubkey::new_unique(),
            ..MockVenue::new(Aggregator::Jupiter, 9_900)
        };
        let too_many_accounts = MockVenue {
            accounts: 70,
            ..MockVenue::new(Aggregator::Titan, 9_900)
        };
        let client = client(vec![
            other_program,
            too_many_accounts,
            MockVenue::new(Aggregator::Dflow, 9_800),
        ]);
        let routes = client
            .build_swap_instructions(&route_request(1_000_000, 0, Sizing::Fixed))
            .await
            .unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].aggregator, Aggregator::Dflow);

        let client = client_without_dflow();
        let error = client
            .build_swap_instructions(&route_request(1_000_000, 0, Sizing::Fixed))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Could not build swap instructions on any venue"),
            "{}",
            error
        );
        assert!(error.contains("calls program"), "{}", error);
        assert!(error.contains("adds 71 accounts"), "{}", error);
    }

    fn client_without_dflow() -> DexSuperAggClient {
        client(vec![
            MockVenue {
                program: Pubkey::new_unique(),
                ..MockVenue::new(Aggregator::Jupiter, 9_900)
            },
            MockVenue {
                accounts: 70,
                ..MockVenue::new(Aggregator::Titan, 9_900)
            },
        ])
    }

    /// Routes for `request` on one 1:1 venue, with the amounts it was asked to quote.
    async fn sized(
        venue: MockVenue,
        request: RouteRequest,
    ) -> (Result<Vec<SwapInstructions>>, Vec<u64>) {
        let quoted = Arc::clone(&venue.quoted);
        let routes = client(vec![venue]).build_swap_instructions(&request).await;
        let quoted = quoted.lock().unwrap().clone();
        (routes, quoted)
    }

    #[tokio::test]
    async fn to_min_out_sizes_the_input_up_until_it_covers() {
        let request = route_request(900_000, 1_000_000, Sizing::ToMinOut { max_in: 1_200_000 });
        let (routes, quoted) = sized(MockVenue::new(Aggregator::Jupiter, 10_000), request).await;
        assert_eq!(quoted, [900_000, 1_000_250]);
        let route = &routes.unwrap()[0];
        assert_eq!(
            (route.in_amount, route.min_out_amount),
            (1_000_250, 1_000_250)
        );
    }

    #[tokio::test]
    async fn to_min_out_sizes_the_input_down_when_it_overshoots() {
        let request = route_request(1_100_000, 1_000_000, Sizing::ToMinOut { max_in: 1_200_000 });
        let (routes, quoted) = sized(MockVenue::new(Aggregator::Jupiter, 10_000), request).await;
        assert_eq!(quoted, [1_100_000, 1_000_250]);
        assert_eq!(routes.unwrap()[0].in_amount, 1_000_250);
    }

    #[tokio::test]
    async fn to_min_out_keeps_the_least_covering_input_when_it_cannot_get_tighter() {
        // The price drops after the first quote: the input sized from it falls short, and the
        // one that is then tight is larger than the first.
        let venue = MockVenue {
            prices: vec![10_000, 9_000],
            ..MockVenue::new(Aggregator::Jupiter, 0)
        };
        let request = route_request(1_100_000, 1_000_000, Sizing::ToMinOut { max_in: 2_000_000 });
        let (routes, quoted) = sized(venue, request).await;
        assert_eq!(quoted, [1_100_000, 1_000_250, 1_111_389]);
        assert_eq!(routes.unwrap()[0].in_amount, 1_100_000);
    }

    #[tokio::test]
    async fn to_min_out_stops_after_the_quote_budget() {
        // Every quote is worse than the last, so no input ever covers.
        let venue = MockVenue {
            prices: vec![9_000, 8_000, 7_000, 6_000, 5_000],
            ..MockVenue::new(Aggregator::Jupiter, 0)
        };
        let request = route_request(1_000_000, 1_000_000, Sizing::ToMinOut { max_in: u64::MAX });
        let (routes, quoted) = sized(venue, request).await;
        assert_eq!(quoted.len(), SIZING_QUOTES);
        assert!(routes.unwrap_err().to_string().starts_with("No route pays"));
    }

    #[tokio::test]
    async fn to_min_out_gives_up_when_the_cap_cannot_cover() {
        let request = route_request(900_000, 1_000_000, Sizing::ToMinOut { max_in: 950_000 });
        let (routes, quoted) = sized(MockVenue::new(Aggregator::Jupiter, 10_000), request).await;
        assert_eq!(quoted, [900_000, 950_000]);
        let error = routes.unwrap_err().to_string();
        assert!(error.contains("for at most 950000"), "{}", error);

        let request = route_request(960_000, 1_000_000, Sizing::ToMinOut { max_in: 950_000 });
        let (routes, quoted) = sized(MockVenue::new(Aggregator::Jupiter, 10_000), request).await;
        assert!(quoted.is_empty());
        let error = routes.unwrap_err().to_string();
        assert!(error.contains("already exceeds the input cap"), "{}", error);
    }

    #[tokio::test]
    async fn to_min_out_keeps_a_covering_input_when_a_later_quote_fails() {
        let quoted = Arc::new(Mutex::new(Vec::new()));
        let venue = MockVenue {
            quoted: Arc::clone(&quoted),
            ..MockVenue::new(Aggregator::Jupiter, 10_000)
        };
        // A second venue that only ever fails keeps the first quote round from failing as a
        // whole; the covering round is kept when the next round has no quote at all.
        let client = client(vec![venue]);
        let request = route_request(1_100_000, 1_000_000, Sizing::ToMinOut { max_in: 1_200_000 });
        let routes = client.build_swap_instructions(&request).await.unwrap();
        assert_eq!(routes[0].in_amount, 1_000_250);
        assert_eq!(quoted.lock().unwrap().len(), 2);
    }

    fn rpc(mocks: MocksMap) -> DexSuperAggClient {
        DexSuperAggClient::with_venues(
            Arc::new(RpcClient::new_mock_with_mocks_map("sig_not_found", mocks)),
            ClientConfig {
                confirm_timeout: Duration::from_secs(3),
                ..Default::default()
            },
            Pubkey::new_unique(),
            vec![],
        )
    }

    fn status(value: Value) -> Value {
        json!({ "context": { "slot": 1 }, "value": [value] })
    }

    fn transaction() -> VersionedTransaction {
        VersionedTransaction {
            signatures: vec![Signature::from([9; 64])],
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_transaction_is_confirmed_once_its_status_is_ok() {
        let mut mocks = MocksMap::default();
        mocks.insert(RpcRequest::GetSignatureStatuses, status(Value::Null));
        mocks.insert(
            RpcRequest::GetSignatureStatuses,
            status(json!({
                "slot": 1, "confirmations": 1, "confirmationStatus": "confirmed",
                "status": { "Ok": null }, "err": null
            })),
        );
        let signature = rpc(mocks)
            .send_and_confirm(&transaction(), CommitmentLevel::Confirmed)
            .await
            .unwrap();
        assert_eq!(signature, Signature::from([9; 64]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_transaction_is_an_error() {
        let mut mocks = MocksMap::default();
        mocks.insert(
            RpcRequest::GetSignatureStatuses,
            status(json!({
                "slot": 1, "confirmations": 1, "confirmationStatus": "confirmed",
                "status": { "Err": "AccountInUse" }, "err": "AccountInUse"
            })),
        );
        let error = rpc(mocks)
            .send_and_confirm(&transaction(), CommitmentLevel::Confirmed)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed: Account in use"), "{}", error);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_blockhash_ends_the_wait() {
        let mut mocks = MocksMap::default();
        for _ in 0..REBROADCAST_EVERY_POLLS + 1 {
            mocks.insert(RpcRequest::GetSignatureStatuses, status(Value::Null));
        }
        mocks.insert(
            RpcRequest::IsBlockhashValid,
            json!({ "context": { "slot": 1 }, "value": false }),
        );
        let error = rpc(mocks)
            .send_and_confirm(&transaction(), CommitmentLevel::Confirmed)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("expired without landing"), "{}", error);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_transaction_times_out() {
        let mut mocks = MocksMap::default();
        for _ in 0..20 {
            mocks.insert(RpcRequest::GetSignatureStatuses, status(Value::Null));
        }
        mocks.insert(
            RpcRequest::IsBlockhashValid,
            json!({ "context": { "slot": 1 }, "value": true }),
        );
        let error = rpc(mocks)
            .send_and_confirm(&transaction(), CommitmentLevel::Confirmed)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Timed out waiting"), "{}", error);
    }
}

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
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// How far a min-out swap may raise its input above the initial amount when the caller passes no
/// `max_in_amount`.
const DEFAULT_MAX_INPUT_BUMP: f64 = 1.10;

/// Margin on top of the observed shortfall ratio when raising a min-out swap's input.
const MIN_OUT_BUMP_MARGIN: f64 = 1.02;

/// Re-quotes a min-out swap may make after its first quote falls short.
const MIN_OUT_MAX_REQUOTES: usize = 2;

const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Rebroadcast the pending transaction every this many polls.
const REBROADCAST_EVERY_POLLS: u32 = 4;

/// Maximum quotes `build_swap_instructions` takes while sizing the input.
const SIZING_QUOTES: usize = 4;

/// Sizing stops once the guaranteed output exceeds `min_out` by at most this much.
pub const SIZING_TOLERANCE_BPS: u64 = 5;

/// One venue's quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueQuote {
    pub aggregator: Aggregator,
    /// Output the venue expects.
    pub out_amount: u64,
    /// Output the venue guarantees after slippage.
    pub min_out_amount: u64,
}

/// A signed swap transaction, not yet submitted.
#[derive(Debug, Clone)]
pub struct SwapTransaction {
    pub transaction: VersionedTransaction,
    pub in_amount: u64,
    /// Output the venue expects. The guaranteed output is at least the requested `min_out`.
    pub out_amount: u64,
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
    /// Output the venue expects.
    pub out_amount: u64,
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

/// Input for the next min-out re-quote: scale by the observed shortfall (plus a margin), always
/// make progress, and never exceed `max_in`.
fn bumped_input(amount: u64, quoted_out: u64, min_out: u64, max_in: u64) -> u64 {
    let ratio = min_out as f64 / quoted_out.max(1) as f64 * MIN_OUT_BUMP_MARGIN;
    let bumped = (amount as f64 * ratio).ceil() as u64; // saturates on overflow
    bumped.max(amount.saturating_add(1)).min(max_in)
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

        Ok(Self {
            rpc_client,
            config,
            user: signer.pubkey(),
            venues,
        })
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
                            out_amount: quote.out_amount,
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
                        out_amount = quote.out_amount,
                        quotes = ?summary,
                        "Selected aggregator"
                    );
                    return Ok(SwapTransaction {
                        transaction,
                        in_amount: amount,
                        out_amount: quote.out_amount,
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
                out_amount: quote.out_amount,
                min_out_amount: quote.min_out_amount,
            })
            .collect())
    }

    /// Builds a signed swap of `amount` that guarantees at least `min_out` after slippage (0 for
    /// no minimum). If the quote falls short, the input is raised and re-quoted, at most
    /// `MIN_OUT_MAX_REQUOTES` times and never above `max_in_amount` (default:
    /// `DEFAULT_MAX_INPUT_BUMP` × `amount`).
    pub async fn build_swap_transaction(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        min_out: u64,
        max_in_amount: Option<u64>,
    ) -> Result<SwapTransaction> {
        let max_in =
            max_in_amount.unwrap_or_else(|| (amount as f64 * DEFAULT_MAX_INPUT_BUMP).ceil() as u64);
        if amount > max_in {
            bail!("input {} already exceeds the input cap {}", amount, max_in);
        }

        let request = self.transaction_request(input_mint, output_mint, amount);
        let mut amount = amount;
        let mut requotes = 0;
        loop {
            let quotes = self.collect_quotes(&request, amount).await?;
            let best_out = quotes.iter().map(|q| q.min_out_amount).max().unwrap_or(0);
            if best_out >= min_out {
                return self.build_best(&quotes, amount, min_out).await;
            }

            if requotes == MIN_OUT_MAX_REQUOTES || amount >= max_in {
                bail!(
                    "Best quote for {} {} guarantees {} {}, short of the {} needed; input is capped at {}",
                    amount,
                    input_mint,
                    best_out,
                    output_mint,
                    min_out,
                    max_in
                );
            }
            let next = bumped_input(amount, best_out, min_out, max_in);
            tracing::debug!(
                from = amount,
                to = next,
                best_out,
                min_out,
                max_in,
                "Raising input"
            );
            amount = next;
            requotes += 1;
        }
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

    #[test]
    fn bump_scales_by_shortfall_with_margin() {
        // Quote delivered 80 of the 100 needed: raise by 100/80 * 1.02.
        assert_eq!(bumped_input(1_000, 80, 100, 10_000), 1_275);
    }

    #[test]
    fn bump_never_exceeds_the_cap() {
        // A 1.6x market premium wants ~1.63x the input; the cap holds it.
        assert_eq!(bumped_input(1_000, 625, 1_000, 1_100), 1_100);
        // A quote of zero would otherwise ask for an absurd input.
        assert_eq!(bumped_input(1_000, 0, 1_000, 1_100), 1_100);
    }

    #[test]
    fn bump_always_makes_progress() {
        // Already within rounding of min_out: still moves up by at least one unit.
        assert_eq!(
            bumped_input(1_000_000, 999_999, 1_000_000, u64::MAX),
            1_020_002
        );
        assert_eq!(bumped_input(10, 10, 10, u64::MAX), 11);
    }

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
}

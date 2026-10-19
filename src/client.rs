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

/// Quotes `build_swap_instructions` may take while sizing its input.
const SIZING_QUOTES: usize = 4;

/// How far above `min_out` the best route's guaranteed output may land before `build_swap_instructions`
/// sizes its input down.
pub const SIZING_TOLERANCE_BPS: u64 = 5;

/// A venue's quote for a swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueQuote {
    pub aggregator: Aggregator,
    /// Expected output.
    pub out_amount: u64,
    /// Output the route guarantees at the configured slippage.
    pub min_out_amount: u64,
}

/// A signed swap transaction, not yet submitted.
#[derive(Debug, Clone)]
pub struct SwapTransaction {
    pub transaction: VersionedTransaction,
    pub in_amount: u64,
    /// Expected output of the quote it was built from; at least `min_out` is guaranteed.
    pub out_amount: u64,
    pub aggregator: Aggregator,
}

/// An exact-in swap for a caller to put in its own transaction.
#[derive(Debug, Clone)]
pub struct RouteRequest {
    /// The wallet the route swaps for, which signs the caller's transaction.
    pub user: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// The input swapped, or the one resizing starts from.
    pub amount: u64,
    /// The least the route must pay once slippage is counted.
    pub min_out: u64,
    pub sizing: Sizing,
    /// Slippage the route is quoted at, which sets the least it pays.
    pub slippage_bps: u16,
    /// Accounts the caller's transaction already references. A route reuses them at no cost.
    pub transaction_accounts: Vec<Pubkey>,
    /// Distinct accounts the route may add to the caller's transaction, programs included.
    pub max_accounts: usize,
    /// Programs the route's top-level instructions may call; `None` allows any.
    pub allowed_programs: Option<Vec<Pubkey>>,
}

impl RouteRequest {
    /// A request to swap exactly `amount` for `user`, with no limits on the route (any program,
    /// up to a transaction's worth of accounts); set `sizing` and the limits as needed.
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

/// How `build_swap_instructions` picks the input it swaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    /// Swap exactly `amount`.
    Fixed,
    /// Resize from `amount` toward the least input whose best route covers `min_out`, never above
    /// `max_in`.
    ToMinOut { max_in: u64 },
}

/// An exact-in route as instructions for the requesting wallet, for the caller's own transaction.
/// It swaps between the wallet's existing token accounts for the two mints.
#[derive(Debug, Clone)]
pub struct SwapInstructions {
    pub aggregator: Aggregator,
    pub in_amount: u64,
    /// Expected (not slippage-adjusted) output.
    pub out_amount: u64,
    /// The least the route pays; below it the route fails.
    pub min_out_amount: u64,
    /// The route and any token accounts it creates for itself (ATA program): no compute budget,
    /// wrapping or closing. The user's token accounts for both mints must exist.
    pub instructions: Vec<Instruction>,
    /// Lookup tables the venue offers for the route's accounts.
    pub address_lookup_tables: Vec<Pubkey>,
}

/// Routes exact-in swaps across DEX aggregators (Jupiter, Titan, DFlow).
pub struct DexSuperAggClient {
    rpc_client: Arc<RpcClient>,
    config: ClientConfig,
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

/// The input that brings the best route's guaranteed output to `min_out` plus half the sizing
/// tolerance, at the rate of a quote that guaranteed `got` for `amount`; never above `max_in`.
fn sized_input(amount: u64, got: u64, min_out: u64, max_in: u64) -> u64 {
    let target =
        (u128::from(min_out) * (20_000 + u128::from(SIZING_TOLERANCE_BPS))).div_ceil(20_000);
    let sized = (u128::from(amount) * target).div_ceil(u128::from(got.max(1)));
    u64::try_from(sized).unwrap_or(u64::MAX).min(max_in)
}

/// Whether a guaranteed output of `got` covers `min_out` by no more than `SIZING_TOLERANCE_BPS`.
fn within_tolerance(got: u64, min_out: u64) -> bool {
    min_out == 0
        || (got >= min_out
            && u128::from(got) * 10_000
                <= u128::from(min_out) * (10_000 + u128::from(SIZING_TOLERANCE_BPS)))
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

/// What succeeded in `results`, each venue that failed logged as `failed`. Errors with `none` and
/// every venue's failure when nothing succeeded.
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

    /// Run `quote` on every venue in `venues` in parallel. A venue that fails or misses the deadline
    /// is logged and left out; only if none quotes is it an error.
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
            async move {
                let result = match timeout(quote_timeout, quoted).await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("timed out after {:?}", quote_timeout)),
                };
                (venue.kind(), result)
            }
        }))
        .await;

        successes(results, "Quote failed", "No aggregator returned a quote")
    }

    /// Quote every configured venue in parallel, most guaranteed output first. A venue that fails
    /// or misses the deadline is logged and left out; only if none quotes is it an error.
    async fn collect_quotes(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
    ) -> Result<Vec<Quote>> {
        check_swap(input_mint, output_mint, amount)?;
        let mut quotes = self
            .quote_each(self.venues.iter().map(|venue| venue.as_ref()), |venue| {
                venue.quote(input_mint, output_mint, amount)
            })
            .await?;

        // Stable: on a tie the configured order decides.
        quotes.sort_by(|a, b| b.min_out_amount.cmp(&a.min_out_amount));
        Ok(quotes)
    }

    /// Quote `amount` for `request` on every venue in parallel, most guaranteed output first.
    async fn collect_route_quotes(
        &self,
        request: &RouteRequest,
        amount: u64,
    ) -> Result<Vec<Quote>> {
        check_swap(&request.input_mint, &request.output_mint, amount)?;
        let mut quotes = self
            .quote_each(self.venues.iter().map(|venue| venue.as_ref()), |venue| {
                venue.route_quote(request, amount)
            })
            .await?;

        quotes.sort_by(|a, b| b.min_out_amount.cmp(&a.min_out_amount));
        Ok(quotes)
    }

    /// The instructions of every quote of `amount` paying at least `min_out`, best first. A venue
    /// that fails to build, or breaks the request's limits, is logged and left out.
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
                            match timeout(build_timeout, venue.route_instructions(request, quote))
                                .await
                            {
                                Ok(built) => built?,
                                Err(_) => bail!("timed out after {:?}", build_timeout),
                            };
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

    /// Build the best quote guaranteeing at least `min_out`, falling back to the next best if its
    /// venue fails to build.
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
            let built =
                match timeout(self.config.build_timeout, venue.build_transaction(quote)).await {
                    Ok(built) => built,
                    Err(_) => Err(anyhow!("timed out after {:?}", self.config.build_timeout)),
                };
            match built {
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
            .collect_quotes(&input_mint, &output_mint, amount)
            .await?
            .into_iter()
            .map(|quote| VenueQuote {
                aggregator: quote.aggregator,
                out_amount: quote.out_amount,
                min_out_amount: quote.min_out_amount,
            })
            .collect())
    }

    /// Build a signed exact-in swap transaction guaranteeing at least `min_out` (0 for any) at the
    /// configured slippage. If the quote for `amount` falls short, the input is raised by the
    /// shortfall and re-quoted (at most `MIN_OUT_MAX_REQUOTES` times), never above
    /// `max_in_amount` (default: `DEFAULT_MAX_INPUT_BUMP` × `amount`).
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

        let mut amount = amount;
        let mut requotes = 0;
        loop {
            let quotes = self
                .collect_quotes(&input_mint, &output_mint, amount)
                .await?;
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

    /// Exact-in routes for `request.user` that pay at least `min_out` once slippage is counted, best
    /// first, for the input `request.sizing` picks.
    pub async fn build_swap_instructions(
        &self,
        request: &RouteRequest,
    ) -> Result<Vec<SwapInstructions>> {
        let (covering, max_in) = match request.sizing {
            Sizing::Fixed => {
                let quotes = self.collect_route_quotes(request, request.amount).await?;
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

    /// The least input up to `max_in` whose best route covers `min_out`, with its quotes, resized
    /// from `request.amount` until it covers it by at most `SIZING_TOLERANCE_BPS`. `None` if none
    /// do.
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
            let quotes = match self.collect_route_quotes(request, amount).await {
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

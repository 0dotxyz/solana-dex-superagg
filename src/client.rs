use crate::aggregators::{
    dflow::DflowAggregator, jupiter::JupiterAggregator, titan::TitanAggregator, DexAggregator,
    Quote,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, bail, Result};
use futures_util::future::{join_all, BoxFuture};
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
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

/// A signed swap transaction, not yet submitted.
#[derive(Debug, Clone)]
pub struct SwapTransaction {
    pub transaction: VersionedTransaction,
    pub in_amount: u64,
    /// Expected (not slippage-adjusted) output of the quote it was built from.
    pub out_amount: u64,
    pub aggregator: Aggregator,
}

/// A swap's instructions, for embedding in a transaction of the caller's own (which sets the
/// compute budget, signs and submits).
#[derive(Debug, Clone)]
pub struct SwapInstructions {
    /// Setup (output ATA creation) and swap instructions, in order.
    pub instructions: Vec<Instruction>,
    /// Lookup tables the instructions' accounts come from.
    pub lookup_tables: Vec<Pubkey>,
    pub in_amount: u64,
    /// Expected (not slippage-adjusted) output of the quote they were built from.
    pub out_amount: u64,
    pub aggregator: Aggregator,
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

    /// Quote every configured venue in parallel, best (most output) first. A venue
    /// that fails or misses the deadline is logged and left out; only if none quotes is it an error.
    async fn collect_quotes(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
    ) -> Result<Vec<Quote>> {
        if input_mint == output_mint {
            bail!("Input and output mints cannot be the same");
        }
        if amount == 0 {
            bail!("Swap amount cannot be zero");
        }

        let quote_timeout = self.config.quote_timeout;
        let results = join_all(self.venues.iter().map(|venue| async move {
            let quote =
                match timeout(quote_timeout, venue.quote(input_mint, output_mint, amount)).await {
                    Ok(quote) => quote,
                    Err(_) => Err(anyhow!("timed out after {:?}", quote_timeout)),
                };
            (venue.kind(), quote)
        }))
        .await;

        let mut quotes = Vec::new();
        let mut failures = Vec::new();
        for (aggregator, result) in results {
            match result {
                Ok(quote) => quotes.push(quote),
                Err(e) => {
                    tracing::warn!(aggregator = ?aggregator, error = %e, "Quote failed");
                    failures.push(format!("{:?}: {}", aggregator, e));
                }
            }
        }
        if quotes.is_empty() {
            bail!("No aggregator returned a quote ({})", failures.join("; "));
        }

        // Stable: on a tie the configured order decides.
        quotes.sort_by(|a, b| b.out_amount.cmp(&a.out_amount));
        Ok(quotes)
    }

    /// Build the best quote yielding at least `min_out` with `build`, falling back to the next
    /// best if its venue fails to build.
    async fn build_best<'q, T>(
        &self,
        quotes: &'q [Quote],
        amount: u64,
        min_out: u64,
        build: impl for<'a> Fn(&'a dyn DexAggregator, &'a Quote) -> BoxFuture<'a, Result<T>>,
    ) -> Result<(T, &'q Quote)> {
        let summary: Vec<_> = quotes
            .iter()
            .map(|q| (q.aggregator, q.out_amount))
            .collect();
        let mut failures = Vec::new();
        for quote in quotes.iter().filter(|quote| quote.out_amount >= min_out) {
            let venue = self.venue(quote.aggregator)?;
            let built = match timeout(self.config.build_timeout, build(venue, quote)).await {
                Ok(built) => built,
                Err(_) => Err(anyhow!("timed out after {:?}", self.config.build_timeout)),
            };
            match built {
                Ok(built) => {
                    tracing::info!(
                        aggregator = ?quote.aggregator,
                        in_amount = amount,
                        out_amount = quote.out_amount,
                        quotes = ?summary,
                        "Selected aggregator"
                    );
                    return Ok((built, quote));
                }
                Err(e) => {
                    tracing::warn!(aggregator = ?quote.aggregator, error = %e, "Failed to build swap");
                    failures.push(format!("{:?}: {}", quote.aggregator, e));
                }
            }
        }
        bail!(
            "Could not build a swap on any venue ({})",
            failures.join("; ")
        )
    }

    /// Quotes whose best yields at least `min_out` (0 for any), and the input they are for. If the
    /// quote for `amount` falls short, the input is raised by the shortfall and re-quoted (at most
    /// `MIN_OUT_MAX_REQUOTES` times), never above `max_in_amount` (default:
    /// `DEFAULT_MAX_INPUT_BUMP` × `amount`).
    async fn quotes_for(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
        min_out: u64,
        max_in_amount: Option<u64>,
    ) -> Result<(Vec<Quote>, u64)> {
        let max_in =
            max_in_amount.unwrap_or_else(|| (amount as f64 * DEFAULT_MAX_INPUT_BUMP).ceil() as u64);
        if amount > max_in {
            bail!("input {} already exceeds the input cap {}", amount, max_in);
        }

        let mut amount = amount;
        let mut requotes = 0;
        loop {
            let quotes = self.collect_quotes(input_mint, output_mint, amount).await?;
            let best_out = quotes[0].out_amount;
            if best_out >= min_out {
                return Ok((quotes, amount));
            }

            if requotes == MIN_OUT_MAX_REQUOTES || amount >= max_in {
                bail!(
                    "Best quote for {} {} yields {} {}, short of the {} needed; input is capped at {}",
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

    /// Every venue's expected output for swapping `amount`, best first.
    pub async fn quote(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
    ) -> Result<Vec<(Aggregator, u64)>> {
        Ok(self
            .collect_quotes(&input_mint, &output_mint, amount)
            .await?
            .into_iter()
            .map(|quote| (quote.aggregator, quote.out_amount))
            .collect())
    }

    /// Build a signed exact-in swap transaction yielding at least `min_out` (0 for any), raising
    /// the input up to `max_in_amount` if the quote falls short (see `quotes_for`).
    pub async fn build_swap_transaction(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        min_out: u64,
        max_in_amount: Option<u64>,
    ) -> Result<SwapTransaction> {
        let (quotes, amount) = self
            .quotes_for(&input_mint, &output_mint, amount, min_out, max_in_amount)
            .await?;
        let (transaction, quote) = self
            .build_best(&quotes, amount, min_out, |venue, quote| {
                venue.build_transaction(quote)
            })
            .await?;
        Ok(SwapTransaction {
            transaction,
            in_amount: amount,
            out_amount: quote.out_amount,
            aggregator: quote.aggregator,
        })
    }

    /// Like `build_swap_transaction`, but returns the swap's instructions and lookup tables for
    /// the caller's own transaction.
    pub async fn build_swap_instructions(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
        min_out: u64,
        max_in_amount: Option<u64>,
    ) -> Result<SwapInstructions> {
        let (quotes, amount) = self
            .quotes_for(&input_mint, &output_mint, amount, min_out, max_in_amount)
            .await?;
        let ((instructions, lookup_tables), quote) = self
            .build_best(&quotes, amount, min_out, |venue, quote| {
                venue.build_instructions(quote)
            })
            .await?;
        Ok(SwapInstructions {
            instructions,
            lookup_tables,
            in_amount: amount,
            out_amount: quote.out_amount,
            aggregator: quote.aggregator,
        })
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
}

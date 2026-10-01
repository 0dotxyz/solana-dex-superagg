use crate::aggregators::{
    dflow::DflowAggregator, jupiter::JupiterAggregator, sol, titan::TitanAggregator, DexAggregator,
    Quote, QuoteResult, SwapRequest, SwapResult, SwapSummary, SwapTransaction,
};
use crate::config::{Aggregator, ClientConfig, OutputAtaBehavior, RouteConfig, RoutingStrategy};
use anyhow::{anyhow, bail, Result};
use futures_util::future::join_all;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::{
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use spl_associated_token_account::{
    get_associated_token_address_with_program_id,
    instruction::create_associated_token_account_idempotent,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// How far a min-out swap may raise its input above the caller's initial estimate when the caller
/// doesn't pass its own `max_in_amount`.
pub const DEFAULT_MAX_INPUT_BUMP: f64 = 1.10;

/// Margin on top of the observed shortfall ratio when raising a min-out swap's input.
const MIN_OUT_BUMP_MARGIN: f64 = 1.02;

/// Re-quotes a min-out swap may make after its first quote falls short.
const MIN_OUT_MAX_REQUOTES: usize = 2;

/// Interval between confirmation polls while `swap` waits for its transaction.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Rebroadcast the pending transaction every this many polls.
const REBROADCAST_EVERY_POLLS: u32 = 4;

/// Main client for routing orders across multiple DEX aggregators (Jupiter, Titan, DFlow)
pub struct DexSuperAggClient {
    /// Wallet keypair for signing transactions
    signer: Arc<Keypair>,
    /// Solana RPC client
    rpc_client: Arc<RpcClient>,
    /// Client configuration
    config: ClientConfig,
    jupiter: Option<JupiterAggregator>,
    titan: Option<TitanAggregator>,
    dflow: Option<DflowAggregator>,
}

/// A built transaction plus every quote that was compared to choose it.
struct Built {
    prepared: SwapTransaction,
    quote_results: Vec<(Aggregator, QuoteResult)>,
}

/// Input for the next min-out re-quote: scale by the observed shortfall (plus a margin), always
/// make progress, and never exceed `max_in`.
fn bumped_input(amount: u64, quoted_out: u64, min_out: u64, max_in: u64) -> u64 {
    let ratio = min_out as f64 / quoted_out.max(1) as f64 * MIN_OUT_BUMP_MARGIN;
    let bumped = (amount as f64 * ratio).ceil() as u64; // saturates on overflow
    bumped.max(amount.saturating_add(1)).min(max_in)
}

fn parse_mint(mint: &str) -> Result<Pubkey> {
    Pubkey::from_str(mint).map_err(|e| anyhow!("Invalid mint address {}: {}", mint, e))
}

impl DexSuperAggClient {
    /// Create a new DexSuperAggClient from configuration
    /// Requires a wallet keypair to be configured
    pub fn new(config: ClientConfig) -> Result<Self> {
        let signer = Arc::new(
            config
                .get_keypair()?
                .ok_or_else(|| anyhow!("Wallet keypair is required for DexSuperAggClient"))?,
        );
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            config.shared.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let jupiter = config
            .is_jupiter_configured()
            .then(|| JupiterAggregator::new(&config, Arc::clone(&signer)))
            .transpose()?;
        let titan = config
            .is_titan_configured()
            .then(|| TitanAggregator::new(&config, Arc::clone(&signer), Arc::clone(&rpc_client)))
            .transpose()?;
        let dflow = config
            .is_dflow_configured()
            .then(|| DflowAggregator::new(&config, Arc::clone(&signer)))
            .transpose()?;
        if jupiter.is_none() && titan.is_none() && dflow.is_none() {
            bail!("No DEX aggregator is configured");
        }

        Ok(Self {
            signer,
            rpc_client,
            config,
            jupiter,
            titan,
            dflow,
        })
    }

    /// Create a new DexSuperAggClient from environment variables
    pub fn from_env() -> Result<Self> {
        let config = ClientConfig::from_env()?;
        Self::new(config)
    }

    /// Get the signer's public key
    pub fn pubkey(&self) -> Pubkey {
        self.signer.pubkey()
    }

    /// Get a reference to the configuration
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Get a reference to the RPC client
    pub fn rpc_client(&self) -> &RpcClient {
        &self.rpc_client
    }

    /// Get a reference to the signer
    pub fn signer(&self) -> &Arc<Keypair> {
        &self.signer
    }

    /// Close the Titan WebSocket connection
    /// Call this when you're done with the client to clean up resources
    pub async fn close(&self) -> Result<()> {
        if let Some(titan) = &self.titan {
            titan.close().await?;
        }
        Ok(())
    }

    fn venue(&self, aggregator: Aggregator) -> Result<&dyn DexAggregator> {
        let venue: Option<&dyn DexAggregator> = match aggregator {
            Aggregator::Jupiter => self.jupiter.as_ref().map(|v| v as &dyn DexAggregator),
            Aggregator::Titan => self.titan.as_ref().map(|v| v as &dyn DexAggregator),
            Aggregator::Dflow => self.dflow.as_ref().map(|v| v as &dyn DexAggregator),
        };
        venue.ok_or_else(|| anyhow!("{:?} is not configured", aggregator))
    }

    /// The venues a strategy quotes.
    fn venues(&self, strategy: Option<&RoutingStrategy>) -> Result<Vec<&dyn DexAggregator>> {
        match strategy {
            Some(RoutingStrategy::PreferredAggregator { aggregator }) => {
                Ok(vec![self.venue(*aggregator)?])
            }
            _ => Ok([Aggregator::Jupiter, Aggregator::Titan, Aggregator::Dflow]
                .into_iter()
                .filter_map(|aggregator| self.venue(aggregator).ok())
                .collect()),
        }
    }

    fn slippage_bps(&self, route_config: &RouteConfig) -> u16 {
        route_config
            .slippage_bps
            .unwrap_or(self.config.shared.slippage_bps)
    }

    /// Slippage levels a strategy tries, in order.
    fn slippage_levels(&self, route_config: &RouteConfig) -> Vec<u16> {
        match &route_config.routing_strategy {
            Some(RoutingStrategy::LowestSlippageClimber {
                floor_slippage_bps,
                max_slippage_bps,
                step_bps,
            }) => {
                let mut levels = vec![];
                let mut level = *floor_slippage_bps;
                while level <= *max_slippage_bps {
                    levels.push(level);
                    match level.checked_add((*step_bps).max(1)) {
                        Some(next) => level = next,
                        None => break,
                    }
                }
                levels
            }
            _ => vec![self.slippage_bps(route_config)],
        }
    }

    fn swap_request(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<SwapRequest> {
        let input_mint = parse_mint(input)?;
        let output_mint = parse_mint(output)?;
        if input_mint == output_mint {
            bail!("Input and output mints cannot be the same");
        }
        if amount == 0 {
            bail!("Swap amount cannot be zero");
        }
        Ok(SwapRequest {
            input_mint,
            output_mint,
            amount,
            slippage_bps,
            wrap_and_unwrap_sol: route_config.wrap_and_unwrap_sol,
            compute_unit_price_micro_lamports: route_config.compute_unit_price_micro_lamports,
        })
    }

    /// Quote every venue of the strategy in parallel, best (most output) first. A venue that fails
    /// or misses the quote deadline is logged and left out; only if none quotes is it an error.
    async fn collect_quotes(
        &self,
        request: &SwapRequest,
        strategy: Option<&RoutingStrategy>,
    ) -> Result<Vec<Quote>> {
        let quote_timeout = Duration::from_millis(self.config.shared.quote_timeout_ms);
        let results = join_all(self.venues(strategy)?.into_iter().map(|venue| async move {
            let quote = match timeout(quote_timeout, venue.quote(request)).await {
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

        // Stable: on a tie the configured order (Jupiter, Titan, DFlow) decides.
        quotes.sort_by(|a, b| b.out_amount.cmp(&a.out_amount));
        tracing::debug!(
            input = %request.input_mint,
            output = %request.output_mint,
            amount = request.amount,
            quotes = ?quotes.iter().map(|q| (q.aggregator, q.out_amount)).collect::<Vec<_>>(),
            "Collected quotes"
        );
        Ok(quotes)
    }

    /// Build the best quote yielding at least `min_out`; if its venue fails to build, fall back to
    /// the next best that does.
    async fn build_best(
        &self,
        request: &SwapRequest,
        quotes: Vec<Quote>,
        min_out: u64,
    ) -> Result<Built> {
        let quote_results: Vec<(Aggregator, QuoteResult)> = quotes
            .iter()
            .map(|quote| (quote.aggregator, quote.result.clone()))
            .collect();
        let build_timeout = Duration::from_millis(self.config.shared.build_timeout_ms);

        let mut failures = Vec::new();
        for quote in quotes.iter().filter(|quote| quote.out_amount >= min_out) {
            let venue = self.venue(quote.aggregator)?;
            let built = match timeout(build_timeout, venue.build_transaction(request, quote)).await
            {
                Ok(built) => built,
                Err(_) => Err(anyhow!("timed out after {:?}", build_timeout)),
            };
            match built {
                Ok(transaction) => {
                    tracing::info!(
                        aggregator = ?quote.aggregator,
                        in_amount = request.amount,
                        out_amount = quote.out_amount,
                        quotes = ?quote_results.iter().map(|(a, q)| (*a, q.out_amount)).collect::<Vec<_>>(),
                        "Selected aggregator"
                    );
                    return Ok(Built {
                        prepared: SwapTransaction {
                            transaction,
                            in_amount: request.amount,
                            out_amount: quote.out_amount,
                            slippage_bps_used: Some(request.slippage_bps),
                            aggregator_used: Some(quote.aggregator),
                            quote_result: quote.result.clone(),
                        },
                        quote_results,
                    });
                }
                Err(e) => {
                    tracing::warn!(aggregator = ?quote.aggregator, error = %e, "Failed to build swap transaction");
                    failures.push(format!("{:?}: {}", quote.aggregator, e));
                }
            }
        }

        if failures.is_empty() {
            bail!(
                "No quote yields the required {} (best {})",
                min_out,
                quotes.first().map(|q| q.out_amount).unwrap_or(0)
            );
        }
        bail!(
            "Could not build a swap transaction on any venue ({})",
            failures.join("; ")
        )
    }

    async fn build_at_slippage(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        route_config: &RouteConfig,
    ) -> Result<Built> {
        let request = self.swap_request(input, output, amount, slippage_bps, route_config)?;
        let quotes = self
            .collect_quotes(&request, route_config.routing_strategy.as_ref())
            .await?;
        self.build_best(&request, quotes, 0).await
    }

    /// Quote every venue the default route configuration uses, best first, without building.
    pub async fn quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: &RouteConfig,
    ) -> Result<Vec<(Aggregator, QuoteResult)>> {
        let request = self.swap_request(
            input,
            output,
            amount,
            self.slippage_bps(route_config),
            route_config,
        )?;
        Ok(self
            .collect_quotes(&request, route_config.routing_strategy.as_ref())
            .await?
            .into_iter()
            .map(|quote| (quote.aggregator, quote.result))
            .collect())
    }

    /// Execute a swap using default routing configuration
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    /// * `wrap_and_unwrap_sol` - SOL handling, see [`SwapRequest::wrap_and_unwrap_sol`]
    pub async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapSummary> {
        let mut route_config = self.config.default_route_config();
        route_config.wrap_and_unwrap_sol = wrap_and_unwrap_sol;
        self.swap_with_route_config(input, output, amount, route_config)
            .await
    }

    /// Execute a swap with a custom route configuration and wait for it to reach the configured
    /// commitment. With `LowestSlippageClimber`, a swap that fails is retried at the next slippage
    /// level.
    pub async fn swap_with_route_config(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: RouteConfig,
    ) -> Result<SwapSummary> {
        if route_config.output_ata == OutputAtaBehavior::Create {
            let output_mint = parse_mint(output)?;
            // A native-SOL output has no token account to create.
            if !(route_config.wrap_and_unwrap_sol && output_mint == sol::WSOL_MINT) {
                self.create_ata_if_needed(&output_mint).await?;
            }
        }

        let levels = self.slippage_levels(&route_config);
        let floor = levels.first().copied().unwrap_or_default();
        let mut last_error = None;
        for slippage_bps in levels {
            let start = Instant::now();
            let attempt = async {
                let built = self
                    .build_at_slippage(input, output, amount, slippage_bps, &route_config)
                    .await?;
                let signature = self
                    .send_and_confirm(&built.prepared.transaction, route_config.commitment_level)
                    .await?;
                Ok::<_, anyhow::Error>((built, signature))
            };
            match attempt.await {
                Ok((built, signature)) => {
                    if slippage_bps > floor {
                        tracing::warn!(
                            floor_slippage_bps = floor,
                            actual_slippage_bps = slippage_bps,
                            "Slippage climbed higher than expected"
                        );
                    }
                    return Ok(SwapSummary {
                        swap_result: SwapResult {
                            signature: signature.to_string(),
                            out_amount: built.prepared.out_amount,
                            slippage_bps_used: built.prepared.slippage_bps_used,
                            aggregator_used: built.prepared.aggregator_used,
                            execution_time: Some(start.elapsed()),
                        },
                        quote_results: built.quote_results,
                    });
                }
                Err(e) => {
                    tracing::debug!(slippage_bps, error = %e, "Swap attempt failed");
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("No slippage level to try")))
    }

    /// Build a signed exact-in swap transaction using the default route configuration, without
    /// submitting it.
    pub async fn build_swap_transaction(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapTransaction> {
        let mut route_config = self.config.default_route_config();
        route_config.wrap_and_unwrap_sol = wrap_and_unwrap_sol;
        self.build_swap_transaction_with_route_config(input, output, amount, route_config)
            .await
    }

    /// Build a signed exact-in swap transaction with a custom route configuration, without
    /// submitting it.
    pub async fn build_swap_transaction_with_route_config(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        route_config: RouteConfig,
    ) -> Result<SwapTransaction> {
        let mut last_error = None;
        for slippage_bps in self.slippage_levels(&route_config) {
            match self
                .build_at_slippage(input, output, amount, slippage_bps, &route_config)
                .await
            {
                Ok(built) => return Ok(built.prepared),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("No slippage level to try")))
    }

    /// Build a signed exact-in swap transaction that is expected to yield at least `min_out`.
    ///
    /// See [`Self::build_swap_transaction_for_min_out_with_route_config`].
    pub async fn build_swap_transaction_for_min_out(
        &self,
        input: &str,
        output: &str,
        initial_amount: u64,
        min_out: u64,
        max_in_amount: Option<u64>,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapTransaction> {
        let mut route_config = self.config.default_route_config();
        route_config.wrap_and_unwrap_sol = wrap_and_unwrap_sol;
        self.build_swap_transaction_for_min_out_with_route_config(
            input,
            output,
            initial_amount,
            min_out,
            max_in_amount,
            route_config,
        )
        .await
    }

    /// Build a signed exact-in swap transaction that is expected to yield at least `min_out`, using
    /// a custom route configuration.
    ///
    /// Quotes `initial_amount` first. If no venue yields `min_out`, the input is raised by the
    /// observed shortfall ratio (plus a small margin) and re-quoted, at most
    /// `MIN_OUT_MAX_REQUOTES` times — but never above `max_in_amount`. Pass the most the caller
    /// can afford to spend (e.g. derived from an oracle price); `None` caps it at
    /// [`DEFAULT_MAX_INPUT_BUMP`] × `initial_amount`. The returned transaction's quote always
    /// yields at least `min_out` for at most the cap.
    pub async fn build_swap_transaction_for_min_out_with_route_config(
        &self,
        input: &str,
        output: &str,
        initial_amount: u64,
        min_out: u64,
        max_in_amount: Option<u64>,
        route_config: RouteConfig,
    ) -> Result<SwapTransaction> {
        if initial_amount == 0 {
            bail!("initial input amount cannot be zero when building min-out swap");
        }
        let max_in = max_in_amount
            .unwrap_or_else(|| (initial_amount as f64 * DEFAULT_MAX_INPUT_BUMP).ceil() as u64);
        if initial_amount > max_in {
            bail!(
                "initial input {} already exceeds the input cap {}",
                initial_amount,
                max_in
            );
        }

        // A climber's floor is the tightest level; quotes barely depend on slippage.
        let slippage_bps = self.slippage_levels(&route_config)[0];
        let mut amount = initial_amount;
        let mut requotes = 0;
        loop {
            let request = self.swap_request(input, output, amount, slippage_bps, &route_config)?;
            let quotes = self
                .collect_quotes(&request, route_config.routing_strategy.as_ref())
                .await?;
            let best_out = quotes[0].out_amount;
            if best_out >= min_out {
                return Ok(self.build_best(&request, quotes, min_out).await?.prepared);
            }

            if requotes == MIN_OUT_MAX_REQUOTES || amount >= max_in {
                bail!(
                    "Best quote for {} {} yields {} {}, short of the {} needed; input is capped at {}",
                    amount,
                    input,
                    best_out,
                    output,
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
                "Min-out quote short; raising input"
            );
            amount = next;
            requotes += 1;
        }
    }

    /// Submit `transaction` (e.g. one from `build_swap_transaction*`) and wait until it reaches
    /// `commitment`, rebroadcasting while its blockhash is alive. Fails if the transaction errors,
    /// expires without landing, or doesn't confirm within `shared.confirm_timeout_secs`.
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
        let deadline =
            Instant::now() + Duration::from_secs(self.config.shared.confirm_timeout_secs);
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

    /// Create the wallet's Associated Token Account (ATA) for `mint` if it doesn't exist.
    async fn create_ata_if_needed(&self, mint: &Pubkey) -> Result<()> {
        let owner = self.signer.pubkey();
        let token_program_id = self
            .rpc_client
            .get_account(mint)
            .await
            .map_err(|e| anyhow!("Failed to get mint account {}: {}", mint, e))?
            .owner;
        let ata = get_associated_token_address_with_program_id(&owner, mint, &token_program_id);

        let exists = self
            .rpc_client
            .get_account_with_commitment(&ata, CommitmentConfig::confirmed())
            .await
            .map_err(|e| anyhow!("Failed to look up ATA {}: {}", ata, e))?
            .value
            .is_some();
        if exists {
            return Ok(());
        }

        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|e| anyhow!("Failed to get recent blockhash: {}", e))?;
        let instruction =
            create_associated_token_account_idempotent(&owner, &owner, mint, &token_program_id);
        let message = v0::Message::try_compile(&owner, &[instruction], &[], blockhash)
            .map_err(|e| anyhow!("Failed to compile ATA creation: {}", e))?;
        let transaction =
            VersionedTransaction::try_new(VersionedMessage::V0(message), &[self.signer.as_ref()])
                .map_err(|e| anyhow!("Failed to sign ATA creation: {}", e))?;
        self.send_and_confirm(&transaction, CommitmentLevel::Confirmed)
            .await
            .map_err(|e| anyhow!("Failed to create ATA {}: {}", ata, e))?;
        Ok(())
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

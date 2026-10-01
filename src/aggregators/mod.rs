//! DEX Aggregator trait and implementations
//!
//! A venue quotes, and later builds a transaction from that same quote, so the route compared on
//! price is the route that gets built.

use crate::config::Aggregator;
use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use std::time::Duration;

pub mod dflow;
pub mod jupiter;
pub mod titan;

pub use dflow::DflowAggregator;
pub use jupiter::JupiterAggregator;
pub use titan::TitanAggregator;

/// Result of a swap operation
#[derive(Debug, Clone)]
pub struct SwapResult {
    /// Transaction signature of the executed swap
    pub signature: String,
    /// Output amount received (in lamports/base units)
    pub out_amount: u64,
    /// Slippage tolerance actually used in basis points (e.g., 25 = 0.25%)
    /// This is particularly useful for LowestSlippageClimber strategy to know which slippage level succeeded
    pub slippage_bps_used: Option<u16>,
    /// Aggregator that was used for this swap
    pub aggregator_used: Option<Aggregator>,
    /// Time taken to execute the swap
    pub execution_time: Option<Duration>,
}

/// Result of a quote operation
#[derive(Debug, Clone)]
pub struct QuoteResult {
    /// Expected output amount (in lamports/base units)
    pub out_amount: u64,
    /// Price impact as a percentage (e.g., 0.5 = 0.5% impact); 0 when the venue doesn't report it
    pub price_impact: f64,
    /// Other quote metadata (can be extended with more fields)
    pub metadata: QuoteMetadata,
    /// Time taken for quote
    pub quote_time: Option<Duration>,
}

/// Additional metadata from quote
#[derive(Debug, Clone, Default)]
pub struct QuoteMetadata {
    /// Route information (e.g., "Raydium -> Orca")
    pub route: Option<String>,
    /// Estimated fees
    pub fees: Option<u64>,
    /// Other aggregator-specific data
    pub extra: Option<serde_json::Value>,
}

/// Summary of swap operations including swap result and any quotes that were gathered
#[derive(Debug, Clone)]
pub struct SwapSummary {
    /// The swap result (always present when a swap was executed)
    pub swap_result: SwapResult,
    /// Every quote gathered for the executed attempt, keyed by aggregator
    pub quote_results: Vec<(Aggregator, QuoteResult)>,
}

/// A signed swap transaction prepared by an aggregator but not yet submitted.
#[derive(Debug, Clone)]
pub struct SwapTransaction {
    /// Signed transaction ready to submit, bundle, or simulate.
    pub transaction: VersionedTransaction,
    /// Input amount used to build this exact-in transaction.
    pub in_amount: u64,
    /// Expected output amount from the quote used to build this transaction.
    pub out_amount: u64,
    /// Slippage tolerance used in basis points.
    pub slippage_bps_used: Option<u16>,
    /// Aggregator used to build this transaction.
    pub aggregator_used: Option<Aggregator>,
    /// Quote result used to build this transaction.
    pub quote_result: QuoteResult,
}

/// One exact-in swap, as quoted and built by every venue.
#[derive(Debug, Clone)]
pub struct SwapRequest {
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// Exact input amount in base units.
    pub amount: u64,
    pub slippage_bps: u16,
    /// Same on every venue. `false`: wSOL is swapped from/into the wallet's wSOL ATA (which must
    /// exist for a wSOL input) and the ATA is left open. `true`: native SOL is wrapped/unwrapped
    /// around the swap and the wSOL ATA is closed, unwrapping anything already in it.
    pub wrap_and_unwrap_sol: bool,
    /// Priority fee price; 0 leaves the venue's default.
    pub compute_unit_price_micro_lamports: u64,
}

/// An executable quote from one venue, kept so the same route can be built later.
#[derive(Debug)]
pub struct Quote {
    pub aggregator: Aggregator,
    pub result: QuoteResult,
    pub(crate) payload: QuotePayload,
}

/// The venue-specific data needed to build a transaction from a quote.
#[derive(Debug)]
pub(crate) enum QuotePayload {
    Jupiter(Box<jupiter_swap_api_client::quote::QuoteResponse>),
    Titan(Box<titan::SwapRoute>),
    /// DFlow's quote JSON, passed back to its `/swap` unchanged.
    Dflow(Box<serde_json::Value>),
}

/// A DEX aggregator venue.
#[async_trait]
pub trait DexAggregator: Send + Sync {
    /// Which venue this is.
    fn kind(&self) -> Aggregator;

    /// Fetch an executable exact-in quote for `request`.
    async fn quote(&self, request: &SwapRequest) -> Result<Quote>;

    /// Build and sign a transaction executing `quote`, which this venue produced for `request`.
    async fn build_transaction(
        &self,
        request: &SwapRequest,
        quote: &Quote,
    ) -> Result<VersionedTransaction>;

    /// Release long-lived connections.
    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

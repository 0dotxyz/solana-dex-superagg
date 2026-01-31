//! DEX Aggregator trait and implementations
//!
//! This module provides a trait-based interface for DEX aggregators (Jupiter, Titan, etc.)
//! and their implementations.

use crate::config::Aggregator;
use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::commitment_config::CommitmentLevel;
use std::time::Duration;

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
    /// Aggregator that was used for this swap (Jupiter or Titan)
    /// This is particularly useful for BestPrice and LowestSlippageClimber strategies to see which aggregator was selected
    pub aggregator_used: Option<Aggregator>,
    /// Time taken to execute the swap
    pub execution_time: Option<Duration>,
}

/// Result of a quote operation
#[derive(Debug, Clone)]
pub struct QuoteResult {
    /// Expected output amount (in lamports/base units)
    pub out_amount: u64,
    /// Price impact as a percentage (e.g., 0.5 = 0.5% impact)
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
    /// Quote results that were performed, keyed by aggregator
    /// This allows capturing quotes for all aggregators (e.g., in BestPrice strategy)
    pub quote_results: Vec<(Aggregator, QuoteResult)>,
}

/// Trait that all DEX aggregators must implement
///
/// This trait provides a unified interface for interacting with different DEX aggregators,
/// allowing routing strategies to work with any aggregator implementation.
#[async_trait]
pub trait DexAggregator: Send + Sync {
    /// Execute a swap transaction
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    /// * `slippage_bps` - Slippage tolerance in basis points (e.g., 25 = 0.25%)
    /// * `commitment_level` - Commitment level for transaction confirmation
    /// * `wrap_and_unwrap_sol` - Whether to wrap and unwrap SOL in the swap
    ///
    /// # Returns
    /// `SwapSummary` containing the swap result and (at minimum) the quote used by the aggregator
    async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
        commitment_level: CommitmentLevel,
        wrap_and_unwrap_sol: bool,
    ) -> Result<SwapSummary>;

    /// Quote a swap to get quote information without executing
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    /// * `slippage_bps` - Slippage tolerance in basis points (e.g., 25 = 0.25%)
    ///
    /// # Returns
    /// `QuoteResult` containing expected output amount, price impact, and metadata
    async fn quote(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResult>;
}

pub mod dflow;
pub mod jupiter;
pub mod titan;

pub use dflow::DflowAggregator;
pub use jupiter::JupiterAggregator;
pub use titan::TitanAggregator;

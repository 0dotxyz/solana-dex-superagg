//! DEX Aggregator trait and implementations
//!
//! This module provides a trait-based interface for DEX aggregators (Jupiter, Titan, etc.)
//! and their implementations.

use anyhow::Result;
use async_trait::async_trait;

/// Result of a swap operation
#[derive(Debug, Clone)]
pub struct SwapResult {
    /// Transaction signature of the executed swap
    pub signature: String,
    /// Output amount received (in lamports/base units)
    pub out_amount: u64,
}

/// Result of a simulation/quote operation
#[derive(Debug, Clone)]
pub struct SimulateResult {
    /// Expected output amount (in lamports/base units)
    pub out_amount: u64,
    /// Price impact as a percentage (e.g., 0.5 = 0.5% impact)
    pub price_impact: f64,
    /// Other quote metadata (can be extended with more fields)
    pub metadata: QuoteMetadata,
}

/// Additional metadata from quote/simulation
#[derive(Debug, Clone, Default)]
pub struct QuoteMetadata {
    /// Route information (e.g., "Raydium -> Orca")
    pub route: Option<String>,
    /// Estimated fees
    pub fees: Option<u64>,
    /// Other aggregator-specific data
    pub extra: Option<serde_json::Value>,
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
    ///
    /// # Returns
    /// `SwapResult` containing the transaction signature and output amount
    async fn swap(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<SwapResult>;

    /// Simulate a swap to get quote information without executing
    ///
    /// # Arguments
    /// * `input` - Input token mint address (as string)
    /// * `output` - Output token mint address (as string)
    /// * `amount` - Input amount in lamports/base units
    /// * `slippage_bps` - Slippage tolerance in basis points (e.g., 25 = 0.25%)
    ///
    /// # Returns
    /// `SimulateResult` containing expected output amount, price impact, and metadata
    async fn simulate(
        &self,
        input: &str,
        output: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<SimulateResult>;
}

pub mod jupiter;
pub mod titan;

pub use jupiter::JupiterAggregator;
pub use titan::TitanAggregator;

//! A venue quotes, and later builds a transaction from that same quote, so the route compared on
//! price is the route that gets built.
//!
//! wSOL is always an ordinary SPL token: swapped from/into the wallet's wSOL ATA (which must exist
//! for a wSOL input), never wrapped, unwrapped or closed.

use crate::config::Aggregator;
use anyhow::Result;
use async_trait::async_trait;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

pub(crate) mod dflow;
pub(crate) mod jupiter;
pub(crate) mod titan;

/// An executable exact-in quote, kept so the same route can be built later.
pub(crate) struct Quote {
    pub aggregator: Aggregator,
    /// Expected (not slippage-adjusted) output.
    pub out_amount: u64,
    pub payload: QuotePayload,
}

pub(crate) enum QuotePayload {
    Jupiter(Box<jupiter_swap_api_client::quote::QuoteResponse>),
    Titan(Box<titan::SwapRoute>),
    /// DFlow's quote JSON, passed back to its `/swap` unchanged.
    Dflow(Box<serde_json::Value>),
}

#[async_trait]
pub(crate) trait DexAggregator: Send + Sync {
    fn kind(&self) -> Aggregator;

    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote>;

    /// Build and sign a transaction executing `quote`, which this venue produced.
    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction>;
}

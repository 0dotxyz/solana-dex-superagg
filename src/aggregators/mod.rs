//! A venue quotes, and later builds a transaction from that same quote, so the route compared on
//! price is the route that gets built.
//!
//! wSOL is always an ordinary SPL token: swapped from/into the wallet's wSOL ATA (which must exist
//! for a wSOL input), never wrapped, unwrapped or closed.

use crate::client::{RouteRequest, SwapInstructions};
use crate::config::Aggregator;
use anyhow::Result;
use async_trait::async_trait;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;
use std::collections::HashSet;

pub(crate) mod dflow;
pub(crate) mod jupiter;
pub(crate) mod titan;

/// An executable exact-in quote, kept so the same route can be built later.
pub(crate) struct Quote {
    pub aggregator: Aggregator,
    /// Expected output.
    pub out_amount: u64,
    /// Output the route guarantees at its slippage; it fails on-chain below this.
    pub min_out_amount: u64,
    pub payload: QuotePayload,
}

pub(crate) enum QuotePayload {
    Jupiter(Box<jupiter_swap_api_client::quote::QuoteResponse>),
    Titan(Box<titan::SwapRoute>),
    /// DFlow's quote JSON, passed back to its `/swap` unchanged.
    Dflow(Box<serde_json::Value>),
}

/// An exact-in quote whose route fits a caller's transaction. `route_instructions` builds that
/// same route.
pub(crate) struct RouteQuote {
    pub quote: Quote,
    pub in_amount: u64,
}

#[async_trait]
pub(crate) trait DexAggregator: Send + Sync {
    fn kind(&self) -> Aggregator;

    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote>;

    /// Build and sign a transaction executing `quote`, which this venue produced.
    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction>;

    /// Whether this venue hands routes out as instructions, through `route_quote` and
    /// `route_instructions`.
    fn builds_instructions(&self) -> bool;

    /// An exact-in quote of `amount` for `request`, whose route fits the request's limits.
    async fn route_quote(&self, request: &RouteRequest, amount: u64) -> Result<RouteQuote>;

    /// The instructions of `quote`, which this venue produced with `route_quote` for `request`.
    async fn route_instructions(
        &self,
        request: &RouteRequest,
        quote: &RouteQuote,
    ) -> Result<SwapInstructions>;
}

/// Why `instructions` do not fit `request`'s limits, or `None` when they do.
pub(crate) fn misfit(instructions: &[Instruction], request: &RouteRequest) -> Option<String> {
    if let Some(allowed) = &request.allowed_programs {
        if let Some(program) = instructions
            .iter()
            .map(|instruction| instruction.program_id)
            .find(|program| !allowed.contains(program))
        {
            return Some(format!("calls program {}", program));
        }
    }
    let accounts = instructions
        .iter()
        .flat_map(|instruction| {
            std::iter::once(instruction.program_id)
                .chain(instruction.accounts.iter().map(|meta| meta.pubkey))
        })
        .filter(|account| !request.transaction_accounts.contains(account))
        .collect::<HashSet<_>>()
        .len();
    (accounts > request.max_accounts).then(|| {
        format!(
            "adds {} accounts, over the {} allowed",
            accounts, request.max_accounts
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Sizing;
    use solana_instruction::AccountMeta;

    fn instruction(program: Pubkey, accounts: &[Pubkey]) -> Instruction {
        Instruction {
            program_id: program,
            accounts: accounts
                .iter()
                .map(|account| AccountMeta::new(*account, false))
                .collect(),
            data: vec![1; 8],
        }
    }

    #[test]
    fn a_route_fits_while_it_calls_allowed_programs_within_the_account_limit() {
        let (program, a, b) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let route = [instruction(program, &[a, b]), instruction(program, &[b])];
        let limits = |transaction_accounts, max_accounts, allowed_programs| RouteRequest {
            user: a,
            input_mint: Pubkey::new_unique(),
            output_mint: Pubkey::new_unique(),
            amount: 1,
            min_out: 1,
            sizing: Sizing::Fixed,
            slippage_bps: 50,
            transaction_accounts,
            max_accounts,
            allowed_programs,
        };

        assert_eq!(
            misfit(&route, &limits(vec![], 3, Some(vec![program]))),
            None
        );
        assert_eq!(misfit(&route, &limits(vec![], 3, None)), None);
        assert_eq!(
            misfit(&route, &limits(vec![], 2, None)),
            Some("adds 3 accounts, over the 2 allowed".to_string())
        );
        // Accounts the transaction already references cost nothing.
        assert_eq!(misfit(&route, &limits(vec![a, b], 1, None)), None);
        assert_eq!(
            misfit(&route, &limits(vec![], 3, Some(vec![a]))),
            Some(format!("calls program {}", program))
        );
    }
}

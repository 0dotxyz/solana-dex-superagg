//! Venue (DEX aggregator) integrations. A venue first quotes, then builds exactly the route it
//! quoted, so the price we compared is the price we get.
//!
//! wSOL is treated as a plain SPL token: it is swapped from/into the wallet's wSOL ATA (which must
//! already exist) and never wrapped, unwrapped or closed.

use crate::client::RouteRequest;
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

/// A quote from one venue, with whatever that venue needs to build the same route later.
pub(crate) struct Quote {
    pub aggregator: Aggregator,
    /// Output the venue expects.
    pub out_amount: u64,
    /// Output the venue guarantees after slippage. The route fails on-chain below it.
    pub min_out_amount: u64,
    pub payload: QuotePayload,
}

/// Venue-specific data needed to build the quoted route.
pub(crate) enum QuotePayload {
    Jupiter(Box<jupiter_swap_api_client::quote::QuoteResponse>),
    Titan(Box<titan::SwapRoute>),
    /// DFlow's quote JSON, sent back to its swap endpoints as is.
    Dflow(Box<serde_json::Value>),
}

/// A quoted route as instructions, for the caller's own transaction.
pub(crate) struct Route {
    /// The swap instructions, plus any token accounts the route creates for itself. No compute
    /// budget and no wrapping or unwrapping.
    pub instructions: Vec<Instruction>,
    /// Lookup tables that cover the route's accounts.
    pub address_lookup_tables: Vec<Pubkey>,
}

#[async_trait]
pub(crate) trait DexAggregator: Send + Sync {
    fn kind(&self) -> Aggregator;

    /// Quote swapping `amount` of `input_mint` into `output_mint` at the configured slippage.
    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote>;

    /// Build and sign a transaction for a quote this venue returned from `quote`.
    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction>;

    /// Quote swapping `amount` for `request` at the request's slippage. A venue that can apply the
    /// request's limits while quoting does so here; the client checks them again after building.
    async fn route_quote(&self, request: &RouteRequest, amount: u64) -> Result<Quote>;

    /// Build the instructions for a quote this venue returned from `route_quote`.
    async fn route_instructions(&self, request: &RouteRequest, quote: &Quote) -> Result<Route>;
}

/// Checks `instructions` against the request's limits (allowed programs, account budget).
/// Returns why they don't fit, or `None` if they do.
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

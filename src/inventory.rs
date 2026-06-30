//! Helpers for inventory-style swap sizing.
//!
//! These utilities are intentionally pure: callers can decide whether to use the resulting
//! shortfall with Jupiter, Titan, DFlow, or another route selected by the super aggregator.

use fixed::types::I80F48;

/// Amount still needed after accounting for tokens already held in the wallet.
pub fn shortfall(required_amount: u64, wallet_balance: u64) -> u64 {
    required_amount.saturating_sub(wallet_balance)
}

/// Amount of a token to buy before an inventory-funded action.
///
/// `repay_amount` is truncated to `u64` before subtracting the wallet balance, matching the
/// liquidation repay instruction semantics used by callers that carry amounts as `I80F48`.
pub fn buy_shortfall(repay_amount: I80F48, wallet_balance: u64) -> u64 {
    shortfall(repay_amount.to_num::<u64>(), wallet_balance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortfall_returns_missing_amount() {
        assert_eq!(shortfall(1_000, 600), 400);
    }

    #[test]
    fn shortfall_saturates_when_wallet_covers_requirement() {
        assert_eq!(shortfall(1_000, 1_000), 0);
        assert_eq!(shortfall(1_000, 1_500), 0);
    }

    #[test]
    fn buy_shortfall_handles_empty_wallet() {
        assert_eq!(buy_shortfall(I80F48::from_num(1_000), 0), 1_000);
    }

    #[test]
    fn buy_shortfall_returns_partial_shortfall() {
        assert_eq!(buy_shortfall(I80F48::from_num(1_000), 600), 400);
    }

    #[test]
    fn buy_shortfall_truncates_like_repay() {
        assert_eq!(buy_shortfall(I80F48::from_num(1000.9), 0), 1_000);
    }
}

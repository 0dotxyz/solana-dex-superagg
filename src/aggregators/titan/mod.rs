//! Titan aggregator implementation
//!
//! This module implements the `DexAggregator` trait for Titan protocol.

mod client;
mod transaction_builder;
mod types;

use crate::aggregators::{
    sol, DexAggregator, Quote, QuoteMetadata, QuotePayload, QuoteResult, SwapRequest,
};
use crate::config::{Aggregator, ClientConfig};
use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use client::TitanClient;
pub use types::SwapRoute;

use self::types::{QuoteUpdateParams, SwapMode, SwapParams, SwapQuoteRequest, TransactionParams};

/// Titan aggregator implementation
pub struct TitanAggregator {
    /// Titan WebSocket client (one shared connection, reconnected on demand)
    titan_client: TitanClient,
    /// RPC client for blockhashes and lookup tables
    rpc_client: Arc<RpcClient>,
    /// Wallet keypair for signing transactions
    signer: Arc<Keypair>,
}

impl TitanAggregator {
    /// Create a new Titan aggregator from ClientConfig. Connects lazily, on the first quote.
    pub fn new(
        config: &ClientConfig,
        signer: Arc<Keypair>,
        rpc_client: Arc<RpcClient>,
    ) -> Result<Self> {
        let titan_config = config
            .titan
            .as_ref()
            .ok_or_else(|| anyhow!("Titan configuration not provided"))?;
        let titan_api_key = titan_config
            .titan_api_key
            .as_ref()
            .ok_or_else(|| anyhow!("Titan API key must be provided"))?;
        if titan_api_key.is_empty() {
            return Err(anyhow!("TITAN_API_KEY is set but empty"));
        }

        let titan_client = TitanClient::new(
            titan_config.titan_ws_endpoint.clone(),
            titan_api_key.clone(),
        )
        .with_request_timeout(Duration::from_millis(config.shared.quote_timeout_ms));

        Ok(Self {
            titan_client,
            rpc_client,
            signer,
        })
    }

    /// Close the WebSocket connection
    pub async fn close(&self) -> Result<()> {
        self.titan_client
            .close()
            .await
            .map_err(|e| anyhow!("Failed to close Titan connection: {}", e))
    }
}

/// Whether we wrap the input ourselves: Titan swaps a wSOL input out of the wSOL ATA as-is.
fn wraps_input(request: &SwapRequest) -> bool {
    request.wrap_and_unwrap_sol && request.input_mint == sol::WSOL_MINT
}

fn quote_request(request: &SwapRequest, user: &solana_sdk::pubkey::Pubkey) -> SwapQuoteRequest {
    SwapQuoteRequest {
        swap: SwapParams {
            input_mint: request.input_mint.to_bytes(),
            output_mint: request.output_mint.to_bytes(),
            amount: request.amount,
            swap_mode: Some(SwapMode::ExactIn),
            slippage_bps: Some(request.slippage_bps),
            dexes: None,
            exclude_dexes: None,
            only_direct_routes: None,
            add_size_constraint: None,
            size_constraint: None,
            providers: None,
            accounts_limit_total: None,
            accounts_limit_writable: None,
        },
        transaction: TransactionParams {
            user_public_key: user.to_bytes(),
            // Only when we wrapped the input: closing the ATA returns the lamports.
            close_input_token_account: Some(wraps_input(request)),
            create_output_token_account: Some(true),
            fee_account: None,
            fee_bps: None,
            fee_from_input_mint: None,
            output_account: None,
            // Titan unwraps a wSOL output by default; keep it as wSOL unless asked to unwrap.
            output_wsol: Some(!request.wrap_and_unwrap_sol),
        },
        update: Some(QuoteUpdateParams {
            interval_ms: Some(1000),
            num_quotes: Some(5),
        }),
    }
}

#[async_trait::async_trait]
impl DexAggregator for TitanAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Titan
    }

    async fn quote(&self, request: &SwapRequest) -> Result<Quote> {
        if request.input_mint == request.output_mint {
            return Err(anyhow!("Input and output mints cannot be the same"));
        }

        let start_time = Instant::now();
        let (provider, route) = self
            .titan_client
            .request_swap_quotes(quote_request(request, &self.signer.pubkey()))
            .await
            .map_err(|e| anyhow!("Titan quote failed: {}", e))?;

        let result = QuoteResult {
            out_amount: route.out_amount,
            // Titan doesn't report price impact.
            price_impact: 0.0,
            metadata: QuoteMetadata {
                route: Some(format!(
                    "Titan/{} route with {} steps",
                    provider,
                    route.steps.len()
                )),
                fees: route.platform_fee.as_ref().map(|f| f.amount),
                extra: None,
            },
            quote_time: Some(start_time.elapsed()),
        };

        Ok(Quote {
            aggregator: Aggregator::Titan,
            out_amount: route.out_amount,
            result,
            payload: QuotePayload::Titan(Box::new(route)),
        })
    }

    async fn build_transaction(
        &self,
        request: &SwapRequest,
        quote: &Quote,
    ) -> Result<VersionedTransaction> {
        let QuotePayload::Titan(route) = &quote.payload else {
            return Err(anyhow!("Titan cannot build a {:?} quote", quote.aggregator));
        };

        if let Some(expires_at_ms) = route.expires_at_ms {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if now_ms >= expires_at_ms {
                return Err(anyhow!("Titan route expired before it could be built"));
            }
        }

        let payer = self.signer.pubkey();
        let instructions = transaction_builder::route_instructions(
            route,
            &payer,
            request.compute_unit_price_micro_lamports,
            wraps_input(request).then_some(request.amount),
        );
        let (lookup_tables, blockhash) = tokio::try_join!(
            transaction_builder::fetch_lookup_tables(&self.rpc_client, route),
            async {
                self.rpc_client
                    .get_latest_blockhash()
                    .await
                    .map_err(|e| anyhow!("Failed to get blockhash: {}", e))
            }
        )?;
        let message =
            transaction_builder::compile_message(&payer, &instructions, &lookup_tables, blockhash)?;

        VersionedTransaction::try_new(message, &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    fn request(input_mint: Pubkey, output_mint: Pubkey, wrap: bool) -> SwapRequest {
        SwapRequest {
            input_mint,
            output_mint,
            amount: 1_000,
            slippage_bps: 50,
            wrap_and_unwrap_sol: wrap,
            compute_unit_price_micro_lamports: 0,
        }
    }

    #[test]
    fn wsol_stays_in_the_ata_without_wrap() {
        let usdc = Pubkey::new_unique();
        let user = Pubkey::new_unique();

        let sell = request(sol::WSOL_MINT, usdc, false);
        assert!(!wraps_input(&sell));
        let params = quote_request(&sell, &user).transaction;
        assert_eq!(params.close_input_token_account, Some(false));

        let buy = request(usdc, sol::WSOL_MINT, false);
        assert_eq!(
            quote_request(&buy, &user).transaction.output_wsol,
            Some(true)
        );
    }

    #[test]
    fn wrap_uses_native_sol_on_both_sides() {
        let usdc = Pubkey::new_unique();
        let user = Pubkey::new_unique();

        let sell = request(sol::WSOL_MINT, usdc, true);
        assert!(wraps_input(&sell));
        assert_eq!(
            quote_request(&sell, &user)
                .transaction
                .close_input_token_account,
            Some(true)
        );

        let buy = request(usdc, sol::WSOL_MINT, true);
        assert!(!wraps_input(&buy));
        assert_eq!(
            quote_request(&buy, &user).transaction.output_wsol,
            Some(false)
        );
    }
}

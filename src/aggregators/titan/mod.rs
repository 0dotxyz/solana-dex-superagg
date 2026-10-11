mod client;
mod transaction_builder;
mod types;

use crate::aggregators::{DexAggregator, Quote, QuotePayload};
use crate::config::{Aggregator, ClientConfig, TitanConfig};
use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use self::client::TitanClient;
pub(crate) use self::types::SwapRoute;
use self::types::{QuoteUpdateParams, SwapMode, SwapParams, SwapQuoteRequest, TransactionParams};

pub(crate) struct TitanAggregator {
    titan_client: TitanClient,
    /// For blockhashes and lookup tables
    rpc_client: Arc<RpcClient>,
    signer: Arc<Keypair>,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: u64,
}

impl TitanAggregator {
    pub fn new(
        titan: &TitanConfig,
        config: &ClientConfig,
        signer: Arc<Keypair>,
        rpc_client: Arc<RpcClient>,
    ) -> Self {
        Self {
            titan_client: TitanClient::new(
                titan.titan_ws_endpoint.clone(),
                titan.titan_api_key.clone(),
                config.quote_timeout,
            ),
            rpc_client,
            signer,
            slippage_bps: config.slippage_bps,
            compute_unit_price_micro_lamports: config.compute_unit_price_micro_lamports,
        }
    }
}

fn quote_request(
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount: u64,
    slippage_bps: u16,
    user: &Pubkey,
) -> SwapQuoteRequest {
    SwapQuoteRequest {
        swap: SwapParams {
            input_mint: input_mint.to_bytes(),
            output_mint: output_mint.to_bytes(),
            amount,
            swap_mode: Some(SwapMode::ExactIn),
            slippage_bps: Some(slippage_bps),
        },
        transaction: TransactionParams {
            user_public_key: user.to_bytes(),
            close_input_token_account: Some(false),
            create_output_token_account: Some(true),
            output_wsol: Some(true),
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

    async fn quote(&self, input_mint: &Pubkey, output_mint: &Pubkey, amount: u64) -> Result<Quote> {
        let (provider, route) = self
            .titan_client
            .request_swap_quotes(quote_request(
                input_mint,
                output_mint,
                amount,
                self.slippage_bps,
                &self.signer.pubkey(),
            ))
            .await
            .map_err(|e| anyhow!("Titan quote failed: {}", e))?;
        tracing::debug!(provider = %provider, out_amount = route.out_amount, "Titan quote");

        Ok(Quote {
            aggregator: Aggregator::Titan,
            out_amount: route.out_amount,
            payload: QuotePayload::Titan(Box::new(route)),
        })
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
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
        let instructions =
            transaction_builder::route_instructions(route, self.compute_unit_price_micro_lamports);
        let (lookup_tables, blockhash) = tokio::try_join!(
            transaction_builder::fetch_lookup_tables(&self.rpc_client, route),
            async {
                self.rpc_client
                    .get_latest_blockhash()
                    .await
                    .map_err(|e| anyhow!("Failed to get blockhash: {}", e))
            }
        )?;
        let message = v0::Message::try_compile(&payer, &instructions, &lookup_tables, blockhash)
            .map_err(|e| anyhow!("Failed to compile V0 message: {}", e))?;

        VersionedTransaction::try_new(VersionedMessage::V0(message), &[self.signer.as_ref()])
            .map_err(|e| anyhow!("Failed to sign transaction: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsol_stays_in_the_ata() {
        let mint = Pubkey::new_unique();
        let params = quote_request(&mint, &mint, 1, 50, &mint).transaction;
        assert_eq!(params.output_wsol, Some(true));
        assert_eq!(params.close_input_token_account, Some(false));
    }
}

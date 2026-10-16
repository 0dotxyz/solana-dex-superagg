mod client;
mod transaction_builder;
mod types;

use crate::aggregators::{DexAggregator, Quote, QuotePayload};
use crate::config::{Aggregator, ClientConfig, TitanConfig};
use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::{v0, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
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

/// Titan's route enforces `out_amount` less the requested slippage.
fn min_out_amount(out_amount: u64, slippage_bps: u16) -> u64 {
    out_amount - (out_amount as u128 * slippage_bps as u128 / 10_000) as u64
}

fn unexpired_route(quote: &Quote) -> Result<&SwapRoute> {
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
    Ok(route)
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
            min_out_amount: min_out_amount(route.out_amount, self.slippage_bps),
            payload: QuotePayload::Titan(Box::new(route)),
        })
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        let route = unexpired_route(quote)?;
        let payer = self.signer.pubkey();
        let mut instructions = transaction_builder::compute_budget_instructions(
            route,
            self.compute_unit_price_micro_lamports,
        );
        instructions.extend(transaction_builder::route_instructions(route));
        let lookup_table_keys = transaction_builder::lookup_table_keys(route);
        let (lookup_tables, blockhash) = tokio::try_join!(
            transaction_builder::fetch_lookup_tables(&self.rpc_client, &lookup_table_keys),
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

    async fn build_instructions(&self, quote: &Quote) -> Result<(Vec<Instruction>, Vec<Pubkey>)> {
        let route = unexpired_route(quote)?;
        Ok((
            transaction_builder::route_instructions(route),
            transaction_builder::lookup_table_keys(route),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_out_is_out_less_slippage() {
        assert_eq!(min_out_amount(10_000, 50), 9_950);
        assert_eq!(min_out_amount(u64::MAX, 10_000), 0);
    }

    #[test]
    fn wsol_stays_in_the_ata() {
        let mint = Pubkey::new_unique();
        let params = quote_request(&mint, &mint, 1, 50, &mint).transaction;
        assert_eq!(params.output_wsol, Some(true));
        assert_eq!(params.close_input_token_account, Some(false));
    }
}

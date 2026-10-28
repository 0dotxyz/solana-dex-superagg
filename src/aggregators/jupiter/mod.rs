use crate::aggregators::{sign_transaction, DexAggregator, Quote, QuotePayload, Route};
use crate::client::RouteRequest;
use crate::config::{Aggregator, ClientConfig, JupiterConfig};
use anyhow::{anyhow, Result};
use jupiter_swap_api_client::{
    quote::{QuoteRequest, QuoteResponse},
    route_plan_with_metadata::RoutePlanStep,
    swap::{SwapInstructionsResponse, SwapRequest},
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use std::sync::Arc;

pub(crate) struct JupiterAggregator {
    jupiter_client: JupiterSwapApiClient,
    signer: Arc<Keypair>,
    compute_unit_price_micro_lamports: u64,
}

impl JupiterAggregator {
    pub fn new(
        jupiter: &JupiterConfig,
        config: &ClientConfig,
        signer: Arc<Keypair>,
    ) -> Result<Self> {
        Ok(Self {
            jupiter_client: JupiterSwapApiClient::new(
                jupiter.jup_swap_api_url.clone(),
                jupiter.api_key.clone(),
            )?,
            signer,
            compute_unit_price_micro_lamports: config.compute_unit_price_micro_lamports,
        })
    }
}

#[async_trait::async_trait]
impl DexAggregator for JupiterAggregator {
    fn kind(&self) -> Aggregator {
        Aggregator::Jupiter
    }

    async fn build_transaction(&self, quote: &Quote) -> Result<VersionedTransaction> {
        let quote_response = quote.jupiter()?;
        let swap_response = self
            .jupiter_client
            .swap(
                &SwapRequest {
                    user_public_key: self.signer.pubkey().to_bytes().into(),
                    quote_response: quote_response.clone(),
                    config: TransactionConfig {
                        wrap_and_unwrap_sol: false,
                        // CU limit sized to the simulated route instead of the 1.4M default.
                        dynamic_compute_unit_limit: true,
                        compute_unit_price_micro_lamports: (self.compute_unit_price_micro_lamports
                            > 0)
                        .then_some(ComputeUnitPriceMicroLamports::MicroLamports(
                            self.compute_unit_price_micro_lamports,
                        )),
                        ..Default::default()
                    },
                },
                None,
            )
            .await
            .map_err(|e| anyhow!("Jupiter swap failed: {}", e))?;

        if let Some(error) = swap_response.simulation_error {
            return Err(anyhow!("Jupiter swap simulation failed: {:?}", error));
        }

        sign_transaction(
            &swap_response.swap_transaction,
            &self.signer,
            Aggregator::Jupiter,
        )
    }

    async fn quote(&self, request: &RouteRequest, amount: u64) -> Result<Quote> {
        let quote_response = self
            .jupiter_client
            .quote(&QuoteRequest {
                input_mint: request.input_mint.to_bytes().into(),
                output_mint: request.output_mint.to_bytes().into(),
                amount,
                slippage_bps: request.slippage_bps,
                max_accounts: Some(max_accounts(request)),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow!("Jupiter quote failed: {}", e))?;

        Ok(Quote {
            aggregator: Aggregator::Jupiter,
            min_out_amount: quote_response.other_amount_threshold,
            payload: QuotePayload::Jupiter(Box::new(quote_response)),
        })
    }

    async fn build_instructions(&self, request: &RouteRequest, quote: &Quote) -> Result<Route> {
        let quote_response = quote.jupiter()?;
        let response = self
            .jupiter_client
            .swap_instructions(&SwapRequest {
                user_public_key: request.user.to_bytes().into(),
                quote_response: quote_response.clone(),
                config: instructions_config(quote_response),
            })
            .await
            .map_err(|e| anyhow!("Jupiter swap instructions failed: {}", e))?;
        route(response)
    }
}

/// Settings for a route handed out as instructions. A route through a third mint hops through
/// Jupiter's shared token accounts, so it touches no user token account of that mint.
fn instructions_config(quote: &QuoteResponse) -> TransactionConfig {
    TransactionConfig {
        wrap_and_unwrap_sol: false,
        use_shared_accounts: Some(through_a_third_mint(
            quote.input_mint,
            quote.output_mint,
            &quote.route_plan,
        )),
        ..Default::default()
    }
}

/// Whether some step of the route swaps from or into a mint other than `input` and `output`.
fn through_a_third_mint(input: Pubkey, output: Pubkey, route_plan: &[RoutePlanStep]) -> bool {
    route_plan
        .iter()
        .flat_map(|step| [step.swap_info.input_mint, step.swap_info.output_mint])
        .any(|mint| mint != input && mint != output)
}

const JUPITER: Pubkey = solana_pubkey::pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
const ASSOCIATED_TOKEN_PROGRAM: Pubkey =
    solana_pubkey::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const TOKEN_PROGRAMS: [Pubkey; 2] = [
    solana_pubkey::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    solana_pubkey::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),
];

/// Jupiter's limit counts every account of the route and allows at most 64: the caller's room
/// plus the accounts of its transaction that the route is sure to list.
fn max_accounts(request: &RouteRequest) -> usize {
    let reused = always_listed(request)
        .filter(|account| request.transaction_accounts.contains(account))
        .count();
    request.max_accounts.saturating_add(reused).min(64)
}

/// Accounts both Jupiter swap instructions (`route` and `sharedAccountsRoute`) list: the user,
/// the output mint, Jupiter's program and event authority, and the user's token accounts of both
/// mints. A user token account in the transaction tells which token program owns its mint, and
/// moving that mint lists the program too.
fn always_listed(request: &RouteRequest) -> impl Iterator<Item = Pubkey> + '_ {
    let event_authority = Pubkey::find_program_address(&[b"__event_authority"], &JUPITER).0;
    let owned = TOKEN_PROGRAMS.into_iter().flat_map(move |program| {
        let token_accounts = [request.input_mint, request.output_mint].map(|mint| {
            Pubkey::find_program_address(
                &[request.user.as_ref(), program.as_ref(), mint.as_ref()],
                &ASSOCIATED_TOKEN_PROGRAM,
            )
            .0
        });
        let used = token_accounts
            .iter()
            .any(|account| request.transaction_accounts.contains(account));
        token_accounts.into_iter().chain(used.then_some(program))
    });
    [request.user, request.output_mint, JUPITER, event_authority]
        .into_iter()
        .chain(owned)
}

/// The route in a `/swap-instructions` response. We never ask for wSOL unwrapping (cleanup) or a
/// Jito tip ("other"), so either showing up means a response we don't understand. Setup
/// instructions create token accounts the route needs.
fn route(response: SwapInstructionsResponse) -> Result<Route> {
    if response.cleanup_instruction.is_some() || !response.other_instructions.is_empty() {
        return Err(anyhow!(
            "Jupiter returned cleanup/other instructions that are not handed out"
        ));
    }
    let mut instructions = response.setup_instructions;
    instructions.push(response.swap_instruction);

    Ok(Route {
        instructions,
        address_lookup_tables: response.address_lookup_table_addresses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jupiter_swap_api_client::route_plan_with_metadata::SwapInfo;
    use solana_instruction::Instruction;

    fn token_account(owner: Pubkey, program: Pubkey, mint: Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[owner.as_ref(), program.as_ref(), mint.as_ref()],
            &ASSOCIATED_TOKEN_PROGRAM,
        )
        .0
    }

    #[test]
    fn the_account_limit_adds_only_transaction_accounts_every_route_lists() {
        let (user, input, output) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let [token, token_2022] = TOKEN_PROGRAMS;
        let mut request = RouteRequest::new(user, input, output, 1, 0, 50);
        request.max_accounts = 30;
        request.transaction_accounts = vec![Pubkey::new_unique(); 10];
        assert_eq!(max_accounts(&request), 30, "unrelated accounts add nothing");

        request.transaction_accounts.extend([
            user,
            input,
            output,
            token_account(user, token, input),
            token_account(user, token_2022, output),
            token,
            token_2022,
        ]);
        assert_eq!(
            max_accounts(&request),
            30 + 6,
            "the input mint is not sure to be listed"
        );

        request.transaction_accounts = vec![user, token, token_2022];
        assert_eq!(
            max_accounts(&request),
            31,
            "a token program counts only with a user token account it owns"
        );

        request.max_accounts = usize::MAX;
        assert_eq!(max_accounts(&request), 64);
    }

    fn step(input_mint: Pubkey, output_mint: Pubkey) -> RoutePlanStep {
        RoutePlanStep {
            swap_info: SwapInfo {
                input_mint,
                output_mint,
                ..Default::default()
            },
            percent: 100,
            bps: None,
        }
    }

    #[test]
    fn only_a_route_through_a_third_mint_hops_through_shared_accounts() {
        let (usdc, sol, msol) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        assert!(!through_a_third_mint(usdc, sol, &[step(usdc, sol)]));
        assert!(!through_a_third_mint(
            usdc,
            sol,
            &[step(usdc, sol), step(usdc, sol)]
        ));
        assert!(through_a_third_mint(
            usdc,
            msol,
            &[step(usdc, sol), step(sol, msol)]
        ));
        assert!(through_a_third_mint(
            usdc,
            sol,
            &[step(usdc, sol), step(usdc, msol), step(msol, sol)]
        ));
    }

    fn instruction(program: Pubkey) -> Instruction {
        Instruction {
            program_id: program,
            accounts: vec![],
            data: vec![],
        }
    }

    fn response(setup: Vec<Instruction>, swap: Instruction) -> SwapInstructionsResponse {
        SwapInstructionsResponse {
            token_ledger_instruction: None,
            compute_budget_instructions: vec![instruction(Pubkey::new_unique())],
            setup_instructions: setup,
            swap_instruction: swap,
            cleanup_instruction: None,
            other_instructions: vec![],
            address_lookup_table_addresses: vec![Pubkey::new_unique()],
            prioritization_fee_lamports: 0,
            compute_unit_limit: 0,
            prioritization_type: None,
            dynamic_slippage_report: None,
            simulation_error: None,
        }
    }

    #[test]
    fn the_route_is_the_setup_and_swap_instructions_without_the_compute_budget() {
        let (ata, router) = (Pubkey::new_unique(), Pubkey::new_unique());
        let response = response(vec![instruction(ata)], instruction(router));
        let tables = response.address_lookup_table_addresses.clone();
        let built = route(response).unwrap();
        let programs: Vec<_> = built.instructions.iter().map(|ix| ix.program_id).collect();
        assert_eq!(programs, [ata, router]);
        assert_eq!(built.address_lookup_tables, tables);
    }

    #[test]
    fn instructions_we_did_not_ask_for_are_an_error() {
        let router = Pubkey::new_unique();
        let mut with_cleanup = response(vec![], instruction(router));
        with_cleanup.cleanup_instruction = Some(instruction(router));
        assert!(route(with_cleanup).is_err());

        let mut with_tip = response(vec![], instruction(router));
        with_tip.other_instructions = vec![instruction(router)];
        assert!(route(with_tip).is_err());
    }
}

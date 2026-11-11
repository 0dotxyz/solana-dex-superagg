//! Builds swaps on each configured venue and simulates them; nothing is sent. Whole transactions
//! swap 1 USDC for wSOL; instructions swap USDC for mSOL and back, the way a liquidation sells
//! seized collateral inside its own transaction.
//!
//! RPC_URL=… WALLET='[1,2,…]' JUP_API_KEY=… \
//!   TITAN_WS_ENDPOINT=… TITAN_API_KEY=… DFLOW_API_KEY=… \
//!   cargo test --test live -- --ignored --nocapture --test-threads=1
//!
//! The wallet needs 1 USDC, 0.005 mSOL, and token accounts for USDC and mSOL (instructions never
//! create the wallet's own). A venue whose env vars are unset is skipped.

use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_dex_superagg::{
    Aggregator, ClientConfig, DexSuperAggClient, DflowConfig, JupiterConfig, RouteRequest, Sizing,
    SwapInstructions, TitanConfig,
};
use solana_keypair::Keypair;
use solana_message::{v0, v1, AddressLookupTableAccount, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const USDC: Pubkey = Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");
const MSOL: Pubkey = Pubkey::from_str_const("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So");
const JUPITER_PROGRAM: Pubkey =
    Pubkey::from_str_const("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");
const TITAN_PROGRAM: Pubkey = Pubkey::from_str_const("T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT");
const DFLOW_PROGRAM: Pubkey =
    Pubkey::from_str_const("DF1ow4tspfHX9JwWJsAb9epbkA8hmpSEAtxXy1V27QBH");
const ATA_PROGRAM: Pubkey = Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn wallet() -> Keypair {
    let bytes: Vec<u8> = env("WALLET")
        .expect("WALLET is required")
        .trim_matches(['[', ']'])
        .split(',')
        .map(|byte| {
            byte.trim()
                .parse()
                .expect("WALLET must be a JSON byte array")
        })
        .collect();
    Keypair::try_from(bytes.as_slice()).expect("invalid WALLET")
}

/// Config with every venue whose env vars are set.
fn config() -> ClientConfig {
    ClientConfig {
        rpc_url: env("RPC_URL").expect("RPC_URL is required"),
        jupiter: env("JUP_API_KEY").map(|api_key| JupiterConfig {
            jup_swap_api_url: env("JUP_API_URL")
                .unwrap_or_else(|| "https://api.jup.ag/swap/v1".to_string()),
            api_key,
        }),
        titan: env("TITAN_API_KEY").map(|titan_api_key| TitanConfig {
            titan_ws_endpoint: env("TITAN_WS_ENDPOINT").expect("TITAN_WS_ENDPOINT is required"),
            titan_api_key,
        }),
        dflow: env("DFLOW_API_KEY").map(|api_key| DflowConfig {
            api_url: env("DFLOW_API_URL")
                .unwrap_or_else(|| "https://quote-api.dflow.net".to_string()),
            api_key,
        }),
        ..Default::default()
    }
}

/// A client with only `aggregator`, or `None` if it is not configured.
fn only(config: &ClientConfig, aggregator: Aggregator) -> Option<DexSuperAggClient> {
    let configured = match aggregator {
        Aggregator::Jupiter => config.jupiter.is_some(),
        Aggregator::Titan => config.titan.is_some(),
        Aggregator::Dflow => config.dflow.is_some(),
    };
    if !configured {
        println!("{:?}: skipped (not configured)", aggregator);
        return None;
    }
    let client = DexSuperAggClient::new(
        ClientConfig {
            jupiter: config
                .jupiter
                .clone()
                .filter(|_| aggregator == Aggregator::Jupiter),
            titan: config
                .titan
                .clone()
                .filter(|_| aggregator == Aggregator::Titan),
            dflow: config
                .dflow
                .clone()
                .filter(|_| aggregator == Aggregator::Dflow),
            ..config.clone()
        },
        wallet(),
    )
    .unwrap();
    Some(client)
}

async fn assert_simulates(
    rpc: &RpcClient,
    aggregator: Aggregator,
    transaction: &VersionedTransaction,
) {
    let simulation = rpc
        .simulate_transaction_with_config(
            transaction,
            RpcSimulateTransactionConfig {
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        simulation.value.err.is_none(),
        "{:?}: {:?}",
        aggregator,
        simulation.value.logs
    );
}

async fn lookup_tables(rpc: &RpcClient, keys: &[Pubkey]) -> Vec<AddressLookupTableAccount> {
    let accounts = rpc.get_multiple_accounts(keys).await.unwrap();
    keys.iter()
        .zip(accounts)
        .map(|(key, account)| AddressLookupTableAccount {
            key: *key,
            addresses: AddressLookupTable::deserialize(&account.expect("lookup table").data)
                .unwrap()
                .addresses
                .to_vec(),
        })
        .collect()
}

fn route_request(wallet: &Keypair, amount: u64, min_out: u64, sizing: Sizing) -> RouteRequest {
    RouteRequest {
        user: wallet.pubkey(),
        input_mint: USDC,
        output_mint: MSOL,
        amount,
        min_out,
        sizing,
        slippage_bps: 50,
        transaction_accounts: vec![wallet.pubkey()],
        max_accounts: 64,
        allowed_programs: Some(vec![
            JUPITER_PROGRAM,
            TITAN_PROGRAM,
            DFLOW_PROGRAM,
            ATA_PROGRAM,
        ]),
    }
}

async fn assert_route_simulates(rpc: &RpcClient, wallet: &Keypair, route: SwapInstructions) {
    let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
    instructions.extend(route.instructions);
    let message = v0::Message::try_compile(
        &wallet.pubkey(),
        &instructions,
        &lookup_tables(rpc, &route.address_lookup_tables).await,
        rpc.get_latest_blockhash().await.unwrap(),
    )
    .unwrap();
    let transaction =
        VersionedTransaction::try_new(VersionedMessage::V0(message), &[wallet]).unwrap();
    assert_simulates(rpc, route.aggregator, &transaction).await;
}

#[tokio::test]
#[ignore]
async fn builds_a_simulatable_swap_on_every_venue() {
    let config = config();
    let rpc = RpcClient::new(config.rpc_url.clone());

    for aggregator in [Aggregator::Jupiter, Aggregator::Titan, Aggregator::Dflow] {
        let Some(client) = only(&config, aggregator) else {
            continue;
        };
        let swap = client
            .build_swap_transaction(USDC, WSOL, 1_000_000, 0)
            .await
            .unwrap_or_else(|e| panic!("{:?}: {}", aggregator, e));
        println!(
            "{:?}: 1 USDC -> at least {} lamports of wSOL",
            aggregator, swap.min_out_amount
        );
        assert_simulates(&rpc, aggregator, &swap.transaction).await;
    }
}

#[tokio::test]
#[ignore]
async fn sizes_simulatable_swap_instructions_on_every_venue() {
    let config = config();
    let rpc = RpcClient::new(config.rpc_url.clone());
    let wallet = wallet();

    for aggregator in [Aggregator::Jupiter, Aggregator::Titan, Aggregator::Dflow] {
        let Some(client) = only(&config, aggregator) else {
            continue;
        };
        // 99% of what 1 USDC buys, sized from 0.9 USDC.
        let min_out =
            client.quote(USDC, MSOL, 1_000_000).await.unwrap()[0].min_out_amount * 99 / 100;
        let sizing = Sizing::ToMinOut { max_in: 1_000_000 };
        let routes = client
            .build_swap_instructions(&route_request(&wallet, 900_000, min_out, sizing))
            .await
            .unwrap_or_else(|e| panic!("{:?}: {}", aggregator, e));

        for route in routes {
            println!(
                "{:?}: {} USDC base units -> at least {} lamports of mSOL (asked {})",
                route.aggregator, route.in_amount, route.min_out_amount, min_out
            );
            assert!(route.min_out_amount >= min_out, "{:?}", route.aggregator);
            assert_route_simulates(&rpc, &wallet, route).await;
        }
    }
}

#[tokio::test]
#[ignore]
async fn swaps_exactly_a_fixed_input_on_every_venue() {
    let config = config();
    let rpc = RpcClient::new(config.rpc_url.clone());
    let wallet = wallet();

    for aggregator in [Aggregator::Jupiter, Aggregator::Titan, Aggregator::Dflow] {
        let Some(client) = only(&config, aggregator) else {
            continue;
        };
        // Half of what 1 USDC buys, which resizing would shrink the input toward.
        let min_out = client.quote(USDC, MSOL, 1_000_000).await.unwrap()[0].min_out_amount / 2;
        let routes = client
            .build_swap_instructions(&route_request(&wallet, 1_000_000, min_out, Sizing::Fixed))
            .await
            .unwrap_or_else(|e| panic!("{:?}: {}", aggregator, e));

        for route in routes {
            println!(
                "{:?}: {} USDC base units -> at least {} lamports of mSOL",
                route.aggregator, route.in_amount, route.min_out_amount
            );
            assert_route_simulates(&rpc, &wallet, route).await;
        }
    }
}

const TOKEN_PROGRAM: Pubkey = Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

fn token_account(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

/// eva01's sandwich, in shape: the route goes inside a transaction that already holds the
/// wallet's token accounts, may call only the swap programs (no ATA program: the receivership
/// whitelist rejects it), and has room for 40 more accounts. (Jupiter's `maxAccounts` is loose:
/// its routes run a few accounts over it, so much tighter room rejects some of them.) The transaction lands either
/// as v1 over RPC, every address inline, or as v0 with the route's lookup tables in a bundle.
#[tokio::test]
#[ignore]
async fn a_liquidation_shaped_route_simulates_as_v1_and_v0() {
    let config = config();
    let rpc = RpcClient::new(config.rpc_url.clone());
    let wallet = wallet();
    let user = wallet.pubkey();

    for aggregator in [Aggregator::Jupiter, Aggregator::Titan, Aggregator::Dflow] {
        let Some(client) = only(&config, aggregator) else {
            continue;
        };
        // Seized collateral sold for the liability, and the reverse.
        for (input, output, amount) in [(MSOL, USDC, 5_000_000), (USDC, MSOL, 1_000_000)] {
            let request = RouteRequest {
                user,
                input_mint: input,
                output_mint: output,
                amount,
                min_out: 0,
                sizing: Sizing::Fixed,
                slippage_bps: 50,
                transaction_accounts: vec![
                    user,
                    token_account(&user, &input),
                    token_account(&user, &output),
                    TOKEN_PROGRAM,
                    input,
                    output,
                ],
                max_accounts: 40,
                allowed_programs: Some(vec![JUPITER_PROGRAM, TITAN_PROGRAM, DFLOW_PROGRAM]),
            };
            let started = std::time::Instant::now();
            let routes = client
                .build_swap_instructions(&request)
                .await
                .unwrap_or_else(|e| panic!("{:?} {} -> {}: {}", aggregator, input, output, e));
            let route = routes.into_iter().next().unwrap();
            println!(
                "{:?}: {} {} -> at least {} {} in {:?}, {} instruction(s), {} table(s)",
                route.aggregator,
                amount,
                input,
                route.min_out_amount,
                output,
                started.elapsed(),
                route.instructions.len(),
                route.address_lookup_tables.len()
            );
            assert!(
                route
                    .instructions
                    .iter()
                    .all(|ix| ix.program_id != ATA_PROGRAM),
                "{:?}: the route creates the wallet's own token account",
                route.aggregator
            );

            let mut v0_instructions =
                vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
            v0_instructions.extend(route.instructions.iter().cloned());
            let blockhash = rpc.get_latest_blockhash().await.unwrap();
            let v0 = v0::Message::try_compile(
                &user,
                &v0_instructions,
                &lookup_tables(&rpc, &route.address_lookup_tables).await,
                blockhash,
            )
            .unwrap();
            let v0 = VersionedTransaction::try_new(VersionedMessage::V0(v0), &[&wallet]).unwrap();
            assert_simulates(&rpc, route.aggregator, &v0).await;

            let v1 = v1::Message::try_compile_with_config(
                &user,
                &route.instructions,
                blockhash,
                v1::TransactionConfig {
                    compute_unit_limit: Some(1_400_000),
                    loaded_accounts_data_size_limit: Some(64 * 1024 * 1024),
                    ..v1::TransactionConfig::empty()
                },
            )
            .unwrap();
            let v1 = VersionedTransaction::try_new(VersionedMessage::V1(v1), &[&wallet]).unwrap();
            assert_simulates(&rpc, route.aggregator, &v1).await;
        }
    }
}

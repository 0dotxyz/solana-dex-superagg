//! Builds a 1 USDC → wSOL swap on each configured venue, as a transaction and as instructions
//! embedded in our own transaction, and simulates both; nothing is sent.
//!
//! RPC_URL=… WALLET='[1,2,…]' JUP_API_KEY=… TITAN_WS_ENDPOINT=… TITAN_API_KEY=… DFLOW_API_KEY=… \
//!   cargo test --test live -- --ignored --nocapture
//!
//! The wallet needs 1 USDC. A venue whose env vars are unset is skipped.

use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_dex_superagg::{
    Aggregator, ClientConfig, DexSuperAggClient, DflowConfig, JupiterConfig, TitanConfig,
};
use solana_keypair::Keypair;
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const USDC: Pubkey = Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");

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

async fn simulate(rpc: &RpcClient, tx: &VersionedTransaction, what: &str) {
    let simulation = rpc
        .simulate_transaction_with_config(
            tx,
            RpcSimulateTransactionConfig {
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    println!("{}: simulation error: {:?}", what, simulation.value.err);
    assert!(
        simulation.value.err.is_none(),
        "{}: {:?}",
        what,
        simulation.value.logs
    );
}

#[tokio::test]
#[ignore]
async fn builds_a_simulatable_swap_on_every_venue() {
    let rpc_url = env("RPC_URL").expect("RPC_URL is required");
    let config = ClientConfig {
        rpc_url: rpc_url.clone(),
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
    };
    let rpc = RpcClient::new(rpc_url);
    let signer = wallet();

    for aggregator in [Aggregator::Jupiter, Aggregator::Titan, Aggregator::Dflow] {
        let configured = match aggregator {
            Aggregator::Jupiter => config.jupiter.is_some(),
            Aggregator::Titan => config.titan.is_some(),
            Aggregator::Dflow => config.dflow.is_some(),
        };
        if !configured {
            println!("{:?}: skipped (not configured)", aggregator);
            continue;
        }

        // One venue at a time, so each gets exercised rather than only the best quote.
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

        let swap = client
            .build_swap_transaction(USDC, WSOL, 1_000_000, 0, None)
            .await
            .unwrap_or_else(|e| panic!("{:?}: {}", aggregator, e));
        println!(
            "{:?}: 1 USDC -> {} lamports of wSOL",
            aggregator, swap.out_amount
        );
        simulate(
            &rpc,
            &swap.transaction,
            &format!("{:?} transaction", aggregator),
        )
        .await;

        let swap = client
            .build_swap_instructions(USDC, WSOL, 1_000_000, 0, None)
            .await
            .unwrap_or_else(|e| panic!("{:?}: {}", aggregator, e));
        let mut lookup_tables = Vec::new();
        for (key, account) in swap.lookup_tables.iter().zip(
            rpc.get_multiple_accounts(&swap.lookup_tables)
                .await
                .unwrap(),
        ) {
            let account = account.unwrap();
            let table = AddressLookupTable::deserialize(&account.data).unwrap();
            lookup_tables.push(AddressLookupTableAccount {
                key: *key,
                addresses: table.addresses.to_vec(),
            });
        }
        let mut instructions = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
        instructions.extend(swap.instructions);
        let message = v0::Message::try_compile(
            &signer.pubkey(),
            &instructions,
            &lookup_tables,
            rpc.get_latest_blockhash().await.unwrap(),
        )
        .unwrap();
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&signer]).unwrap();
        println!(
            "{:?}: {} instructions, {} lookup tables, {} bytes",
            aggregator,
            instructions.len(),
            lookup_tables.len(),
            bincode::serialize(&tx).unwrap().len()
        );
        simulate(&rpc, &tx, &format!("{:?} instructions", aggregator)).await;
    }
}

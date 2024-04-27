use std::{env, num::NonZeroU64, path::PathBuf, sync::Arc, time::Duration};

use clap::{Parser, Subcommand};
use env_logger::TimestampPrecision;
use futures_util::StreamExt;
use jito_protos::{
    convert::versioned_tx_from_packet,
    searcher::{
        searcher_service_client::SearcherServiceClient,
        ConnectedLeadersRegionedRequest, GetTipAccountsRequest,
        NextScheduledLeaderRequest, PendingTxNotification,
        SubscribeBundleResultsRequest,
    },
};
use jito_searcher_client::{
    get_searcher_client, send_bundle_with_confirmation, token_authenticator::ClientInterceptor,
};
use log::info;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, instruction::{AccountMeta, Instruction}, pubkey::Pubkey, signature::{read_keypair_file, Keypair, Signer}, system_instruction::transfer, sysvar, transaction::{Transaction, VersionedTransaction}
};
use spl_token;
use serum_dex::{instruction::{initialize_market, SelfTradeBehavior, new_order}, error::DexError};
use serum_dex::matching::{Side, OrderType};
use spl_memo::build_memo;
use tokio::time::{sleep, timeout};
use tonic::{codegen::InterceptedService, transport::Channel, Streaming}; 

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// URL of the block engine.
    /// See: https://jito-labs.gitbook.io/mev/searcher-resources/block-engine#connection-details
    #[arg(long, env)]
    block_engine_url: String,

    /// Path to keypair file used to authenticate with the Jito Block Engine
    /// See: https://jito-labs.gitbook.io/mev/searcher-resources/getting-started#block-engine-api-key
    #[arg(long, env)]
    keypair_path: PathBuf,

    /// Comma-separated list of regions to request cross-region data from.
    /// If no region specified, then default to the currently connected block engine's region.
    /// Details: https://jito-labs.gitbook.io/mev/searcher-services/recommendations#cross-region
    /// Available regions: https://jito-labs.gitbook.io/mev/searcher-resources/block-engine#connection-details
    #[arg(long, env, value_delimiter = ',')]
    regions: Vec<String>,

    /// Subcommand to run
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print out information on the next scheduled leader
    NextScheduledLeader,

    /// Prints out information on connected leaders
    ConnectedLeaders,

    /// Prints out connected leaders with their leader slot percentage
    ConnectedLeadersInfo {
        #[clap(long, required = true)]
        rpc_url: String,
    },

    /// Prints out information about the tip accounts
    TipAccounts,

    /// Sends a 1 lamport bundle
    SendBundle {
        /// RPC URL
        #[clap(long, required = true)]
        rpc_url: String,
        /// Filepath to keypair that can afford the transaction payments with 1 lamport tip
        #[clap(long, required = true)]
        payer: PathBuf,
        /// Message you'd like the bundle to say
        #[clap(long, required = true)]
        message: String,
        /// Number of transactions in the bundle (must be <= 5)
        #[clap(long, required = true)]
        num_txs: usize,
        /// Amount of lamports to tip in each transaction
        #[clap(long, required = true)]
        lamports: u64,
        /// One of the tip accounts, see https://jito-foundation.gitbook.io/mev/mev-payment-and-distribution/on-chain-addresses
        #[clap(long, required = true)]
        tip_account: Pubkey,
    },

    /// Initializes a liquidity pool and purchases tokens
    InitializeAndPurchaseToken {
        /// RPC URL
        #[clap(long, required = true)]
        rpc_url: String,
        /// Filepath to keypair of the payer
        #[clap(long, required = true)]
        payer: PathBuf,
        /// Mint address of the token
        #[clap(long, required = true)]
        token_mint_address: Pubkey,
        /// Amount of base token to add to liquidity
        #[clap(long, required = true)]
        base_amount: u64,
        /// Amount of quote token to add to liquidity
        #[clap(long, required = true)]
        quote_amount: u64,
        /// DEX program ID to interact with
        #[clap(long, required = true)]
        dex_program_id: Pubkey,
    },
}

async fn print_next_leader_info(
    client: &mut SearcherServiceClient<InterceptedService<Channel, ClientInterceptor>>,
    regions: Vec<String>,
) {
    let next_leader = client
        .get_next_scheduled_leader(NextScheduledLeaderRequest { regions })
        .await
        .expect("gets next scheduled leader")
        .into_inner();
    println!(
        "next jito-solana slot in {} slots for leader {:?}",
        next_leader.next_leader_slot - next_leader.current_slot,
        next_leader.next_leader_identity
    );
}

#[tokio::main]
async fn main() {
    let args: Args = Args::parse();
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "info")
    }
    env_logger::builder()
        .format_timestamp(Some(TimestampPrecision::Micros))
        .init();

    let keypair = Arc::new(read_keypair_file(&args.keypair_path).expect("reads keypair at path"));
    let mut client = get_searcher_client(&args.block_engine_url, &keypair)
        .await
        .expect("connects to searcher client");

    match args.command {
        Commands::NextScheduledLeader => {
            let next_leader = client
                .get_next_scheduled_leader(NextScheduledLeaderRequest {
                    regions: args.regions,
                })
                .await
                .expect("gets next scheduled leader")
                .into_inner();
            info!(
                "Next leader in {} slots in {}.\
                {next_leader:?}",
                next_leader.next_leader_slot - next_leader.current_slot,
                next_leader.next_leader_region
            );
        }
        Commands::ConnectedLeaders => {
            let connected_leaders = client
                .get_connected_leaders_regioned(ConnectedLeadersRegionedRequest {
                    regions: args.regions,
                })
                .await
                .expect("gets connected leaders")
                .into_inner();
            info!("{connected_leaders:?}");
        }
        Commands::ConnectedLeadersInfo { rpc_url } => {
            let connected_leaders_response = client
                .get_connected_leaders_regioned(ConnectedLeadersRegionedRequest {
                    regions: args.regions,
                })
                .await
                .expect("gets connected leaders")
                .into_inner();
            let connected_validators = connected_leaders_response.connected_validators;

            let rpc_client = RpcClient::new(rpc_url);
            let rpc_vote_account_status = rpc_client
                .get_vote_accounts()
                .await
                .expect("gets vote accounts");

            let total_activated_stake: u64 = rpc_vote_account_status
                .current
                .iter()
                .chain(rpc_vote_account_status.delinquent.iter())
                .map(|vote_account| vote_account.activated_stake)
                .sum();

            let mut total_activated_connected_stake = 0;
            for rpc_vote_account_info in rpc_vote_account_status.current {
                if connected_validators
                    .get(&rpc_vote_account_info.node_pubkey)
                    .is_some()
                {
                    total_activated_connected_stake += rpc_vote_account_info.activated_stake;
                    info!(
                        "connected_leader: {}, stake: {:.2}%",
                        rpc_vote_account_info.node_pubkey,
                        (rpc_vote_account_info.activated_stake * 100) as f64
                            / total_activated_stake as f64
                    );
                }
            }
            info!(
                "total stake for block engine: {:.2}%",
                (total_activated_connected_stake * 100) as f64 / total_activated_stake as f64
            );
        }
        Commands::TipAccounts => {
            let tip_accounts = client
                .get_tip_accounts(GetTipAccountsRequest {})
                .await
                .expect("gets connected leaders")
                .into_inner();
            info!("{:?}", tip_accounts);
        }
        Commands::SendBundle {
            rpc_url,
            payer,
            message,
            num_txs,
            lamports,
            tip_account,
        } => {
            let payer_keypair = read_keypair_file(&payer).expect("reads keypair at path");
            let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
            let balance = rpc_client
                .get_balance(&payer_keypair.pubkey())
                .await
                .expect("reads balance");

            info!(
                "payer public key: {:?} lamports: {balance:?}",
                payer_keypair.pubkey(),
            );

            let mut bundle_results_subscription = client
                .subscribe_bundle_results(SubscribeBundleResultsRequest {})
                .await
                .expect("subscribe to bundle results")
                .into_inner();

            // wait for jito-solana leader slot
            let mut is_leader_slot = false;
            while !is_leader_slot {
                let next_leader = client
                    .get_next_scheduled_leader(NextScheduledLeaderRequest {
                        regions: args.regions.clone(),
                    })
                    .await
                    .expect("gets next scheduled leader")
                    .into_inner();
                let num_slots = next_leader.next_leader_slot - next_leader.current_slot;
                is_leader_slot = num_slots <= 2;
                info!(
                    "next jito leader slot in {num_slots} slots in {}",
                    next_leader.next_leader_region
                );
                sleep(Duration::from_millis(500)).await;
            }

            // build + sign the transactions
            let blockhash = rpc_client
                .get_latest_blockhash()
                .await
                .expect("get blockhash");
            let txs: Vec<_> = (0..num_txs)
                .map(|i| {
                    VersionedTransaction::from(Transaction::new_signed_with_payer(
                        &[
                            build_memo(format!("jito bundle {i}: {message}").as_bytes(), &[]),
                            transfer(&payer_keypair.pubkey(), &tip_account, lamports),
                        ],
                        Some(&payer_keypair.pubkey()),
                        &[&payer_keypair],
                        blockhash,
                    ))
                })
                .collect();

            send_bundle_with_confirmation(
                &txs,
                &rpc_client,
                &mut client,
                &mut bundle_results_subscription,
            )
            .await
            .expect("Sending bundle failed");
        }

        Commands::InitializeAndPurchaseToken {
            rpc_url,
            payer,
            token_mint_address,
            base_amount,
            quote_amount,
            dex_program_id,
        } => {
            let payer_keypair = read_keypair_file(&payer).expect("reads keypair at path");
            let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

            let blockhash = rpc_client.get_latest_blockhash().await.expect("get blockhash");
            let market_pubkey = Pubkey::new_unique();  // example of creating a new market
            let base_mint = token_mint_address;        // already available as per your function arguments
            let quote_mint = Pubkey::new_unique();     // example, needs to be defined based on your specific scenario
            let base_vault = Pubkey::new_unique();     // example
            let quote_vault = Pubkey::new_unique();    // example
            let bids_pubkey = Pubkey::new_unique();    // example
            let asks_pubkey = Pubkey::new_unique();    // example
            let request_queue_pubkey = Pubkey::new_unique(); // example
            let event_queue_pubkey = Pubkey::new_unique();   // example
            let base_lot_size = 100;                  // example value
            let quote_lot_size = 10;                 // example value
            let vault_signer_nonce = 0;              // typically obtained through a program call
            let dust_threshold = 1000;   

            // Build the transaction to initialize liquidity
           let init_liquidity_tx = create_init_liquidity_tx(
                &payer_keypair,
                &dex_program_id,
                &market_pubkey,
                &base_mint,
                &quote_mint,
                &base_vault,
                &quote_vault,
                &bids_pubkey,
                &asks_pubkey,
                &request_queue_pubkey,
                &event_queue_pubkey,
                base_lot_size,
                quote_lot_size,
                vault_signer_nonce,
                dust_threshold,
                &blockhash,
            );

            // Placeholder or example variables (you need to replace or define these correctly based on your context)
            let open_orders_pubkey = Pubkey::new_unique();  // Example, you might have a method to fetch or create this
            let side = Side::Buy;  // Example, set based on purchase intent
            let limit_price = NonZeroU64::new(100).unwrap();  // Example, replace with actual limit price
            let max_coin_qty = NonZeroU64::new(1000).unwrap();  // Example, replace with actual max coin quantity
            let max_native_pc_qty_including_fees = NonZeroU64::new(1000).unwrap(); // Example
            let order_type = OrderType::Limit;  // Example, set based on trading strategy
            let client_order_id = 0;  // Example, unique ID for the order
            let self_trade_behavior = SelfTradeBehavior::DecrementTake;  // Example, set based on policy
            let limit = 10;  // Example, set based on needs
            let srm_account_referral = None;  // Optional, based on your system

            // Build the transaction to purchase the token
            let purchase_tx = create_purchase_tx(
                &payer_keypair,
                &market_pubkey,
                &open_orders_pubkey,
                &request_queue_pubkey,
                &event_queue_pubkey,
                &bids_pubkey,
                &asks_pubkey,
                &base_vault,
                &quote_vault,
                &dex_program_id,
                side,
                limit_price,
                max_coin_qty,
                max_native_pc_qty_including_fees,
                order_type,
                client_order_id,
                self_trade_behavior,
                limit,
                &blockhash,
                srm_account_referral,
            );

            // Combine transactions into a bundle
            let txs = vec![init_liquidity_tx, purchase_tx];

            let mut bundle_results_subscription = client
            .subscribe_bundle_results(SubscribeBundleResultsRequest {})
            .await
            .expect("subscribe to bundle results")
            .into_inner();

            // Send the bundle using Jito
            send_bundle_with_confirmation(
                &txs,
                &rpc_client,
                &mut client,
                &mut bundle_results_subscription,
            )
            .await
            .expect("Sending bundle failed");
        }

    }
}

fn create_init_liquidity_tx(
    payer_keypair: &Keypair,
    dex_program_id: &Pubkey,
    market_pubkey: &Pubkey,
    base_mint: &Pubkey,
    quote_mint: &Pubkey,
    base_vault: &Pubkey,
    quote_vault: &Pubkey,
    bids_pubkey: &Pubkey,
    asks_pubkey: &Pubkey,
    request_queue_pubkey: &Pubkey,
    event_queue_pubkey: &Pubkey,
    base_lot_size: u64,
    quote_lot_size: u64,
    vault_signer_nonce: u64,
    dust_threshold: u64,
    recent_blockhash: &Hash,
) -> Result<Transaction, DexError>{
    let authority = None;  // No authority provided, adapt based on your setup
    let prune_authority = None;  // No prune authority provided
    let consume_events_authority = None;  // No consume events authority provided

    let accounts = vec![
        AccountMeta::new_readonly(*market_pubkey, false),
        AccountMeta::new_readonly(*dex_program_id, false),
        AccountMeta::new_readonly(*base_mint, false),
        AccountMeta::new_readonly(*quote_mint, false),
        AccountMeta::new_readonly(*base_vault, false),
        AccountMeta::new_readonly(*quote_vault, false),
        AccountMeta::new_readonly(payer_keypair.pubkey(), true),
        AccountMeta::new_readonly(*bids_pubkey, false),
        AccountMeta::new_readonly(*asks_pubkey, false),
        AccountMeta::new_readonly(*request_queue_pubkey, false),
        AccountMeta::new_readonly(*event_queue_pubkey, false),
        AccountMeta::new_readonly(sysvar::rent::id(), false),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    let instruction = initialize_market(
        market_pubkey,
        dex_program_id,
        base_mint,
        quote_mint,
        base_vault,
        quote_vault,
        authority.as_ref(),
        prune_authority.as_ref(),
        consume_events_authority.as_ref(),
        bids_pubkey,
        asks_pubkey,
        request_queue_pubkey,
        event_queue_pubkey,
        base_lot_size,
        quote_lot_size,
        vault_signer_nonce,
        dust_threshold,
    );

    instruction.map(|instruction| {
        Transaction::new_signed_with_payer(
            &[Instruction::new_with_bincode(
                *dex_program_id,
                &instruction,
                accounts,
            )],
            Some(&payer_keypair.pubkey()),
            &[payer_keypair],
            *recent_blockhash,
        )
    })
}

fn create_purchase_tx(
    payer_keypair: &Keypair,
    market_pubkey: &Pubkey,
    open_orders_pubkey: &Pubkey,
    request_queue_pubkey: &Pubkey,
    event_queue_pubkey: &Pubkey,
    bids_pubkey: &Pubkey,
    asks_pubkey: &Pubkey,
    base_vault: &Pubkey,
    quote_vault: &Pubkey,
    dex_program_id: &Pubkey,
    side: Side,
    limit_price: NonZeroU64,
    max_coin_qty: NonZeroU64,
    max_native_pc_qty_including_fees: NonZeroU64,
    order_type: OrderType,
    client_order_id: u64,
    self_trade_behavior: SelfTradeBehavior,
    limit: u16,
    recent_blockhash: &Hash,
    srm_account_referral: Option<&Pubkey>,
) -> Result<Transaction, DexError>{
    let accounts = [
        AccountMeta::new(*market_pubkey, false),
        AccountMeta::new(*open_orders_pubkey, false),
        AccountMeta::new(*request_queue_pubkey, false),
        AccountMeta::new(*event_queue_pubkey, false),
        AccountMeta::new(*bids_pubkey, false),
        AccountMeta::new(*asks_pubkey, false),
        AccountMeta::new(*base_vault, false),
        AccountMeta::new(*quote_vault, false),
        AccountMeta::new_readonly(payer_keypair.pubkey(), true), // Correct use here
        AccountMeta::new_readonly(*open_orders_pubkey, true), // Check if this is required twice
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new_readonly(sysvar::rent::id(), false),
    ];

    let instruction = new_order(
        market_pubkey,
        open_orders_pubkey,
        request_queue_pubkey,
        event_queue_pubkey,
        bids_pubkey,
        asks_pubkey,
        &payer_keypair.pubkey(), // Correct use here
        &payer_keypair.pubkey(), // Assume payer is the owner; adjust if different
        base_vault,
        quote_vault,
        &spl_token::id(),
        &sysvar::rent::id(),
        srm_account_referral,
        dex_program_id,
        side,
        limit_price,
        max_coin_qty,
        order_type,
        client_order_id,
        self_trade_behavior,
        limit,
        max_native_pc_qty_including_fees,
    );

    instruction.map(|inst| {
        Transaction::new_signed_with_payer(
            &[Instruction::new_with_bincode(
                *dex_program_id,
                &inst,
                accounts,
            )],
            Some(&payer_keypair.pubkey()),
            &[payer_keypair],
            *recent_blockhash,
        )
    })
}

async fn print_packet_stream(
    client: &mut SearcherServiceClient<InterceptedService<Channel, ClientInterceptor>>,
    mut pending_transactions: Streaming<PendingTxNotification>,
    regions: Vec<String>,
) {
    loop {
        match timeout(Duration::from_secs(5), pending_transactions.next()).await {
            Ok(Some(Ok(notification))) => {
                let transactions: Vec<VersionedTransaction> = notification
                    .transactions
                    .iter()
                    .filter_map(versioned_tx_from_packet)
                    .collect();
                for tx in transactions {
                    info!("tx sig: {:?}", tx.signatures[0]);
                }
            }
            Ok(Some(Err(e))) => {
                info!("error from pending transaction stream: {e:?}");
                break;
            }
            Ok(None) => {
                info!("pending transaction stream closed");
                break;
            }
            Err(_) => {
                print_next_leader_info(client, regions.clone()).await;
            }
        }
    }
}

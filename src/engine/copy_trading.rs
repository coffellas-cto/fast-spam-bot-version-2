use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use anyhow::Result;
use anchor_client::solana_sdk::signature::Signature;
use bs58;
use colored::Colorize;
use tokio::time;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions,  SubscribeUpdate, SubscribeUpdateTransaction,
};
use crate::engine::transaction_parser;
use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
};
use crate::engine::swap::SwapProtocol;
use crate::engine::parallel_processor::ParallelTransactionProcessor;
use crate::services::token_monitor::TokenMonitor;
use dashmap::DashMap;
use crate::common::cache::{PER_ADDRESS_COPY_RATE, COPY_POSITIONS, UPCOMING_BUY_SOL, UPCOMING_SELL_TOKENS, CopyPosition, SPECIAL_TARGET_WALLETS, SPECIAL_TARGET_WALLET_BOUGHT};
use std::collections::{HashMap, HashSet};

// Global state for copy trading - simplified to only track statistics
lazy_static::lazy_static! {
    static ref BOUGHT_COUNT: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref SOLD_COUNT: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref LAST_BUY_TIME: Arc<DashMap<(), Option<Instant>>> = Arc::new(DashMap::new());
    static ref BUYING_ENABLED: Arc<DashMap<(), bool>> = Arc::new(DashMap::new());
}

// Initialize the global counters with default values
fn init_global_state() {
    BOUGHT_COUNT.insert((), 0);
    SOLD_COUNT.insert((), 0);
    LAST_BUY_TIME.insert((), None);
    BUYING_ENABLED.insert((), true);
}

/// Get current statistics
fn get_trading_stats() -> (u64, u64) {
    let bought = BOUGHT_COUNT.get(&()).map(|ref_val| *ref_val).unwrap_or(0);
    let sold = SOLD_COUNT.get(&()).map(|ref_val| *ref_val).unwrap_or(0);
    (bought, sold)
}

/// Log current trading statistics
fn log_trading_stats(logger: &Logger) {
    let (bought, sold) = get_trading_stats();
    logger.log(format!("Trading Stats - Bought: {}, Sold: {}", bought, sold).blue().to_string());
}

/// Reset statistics (useful for manual intervention)
pub fn reset_stats() {
    BOUGHT_COUNT.insert((), 0);
    SOLD_COUNT.insert((), 0);
    println!("Trading statistics reset successfully");
}

/// Get current trading statistics (public function)
pub fn get_current_trading_stats() -> (u64, u64) {
    get_trading_stats()
}

/// Configuration for copy trading - removed counter_limit
pub struct CopyTradingConfig {
    pub yellowstone_grpc_http: String,
    pub yellowstone_grpc_token: String,
    pub app_state: AppState,
    pub swap_config: SwapConfig,
    pub target_addresses: Vec<String>,
    pub excluded_addresses: Vec<String>,
    pub protocol_preference: SwapProtocol,
    pub min_dev_buy: f64,
    pub max_dev_buy: f64,
    pub transaction_landing_mode: crate::common::config::TransactionLandingMode,
}

/// Helper to send heartbeat pings to maintain connection
async fn send_heartbeat_ping(
    subscribe_tx: &Arc<tokio::sync::Mutex<impl Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
) -> Result<(), String> {
    let ping_request = SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 0 }),
        ..Default::default()
    };
    
    let mut tx = subscribe_tx.lock().await;
    match tx.send(ping_request).await {
        Ok(_) => {
            Ok(())
        },
        Err(e) => Err(format!("Failed to send ping: {:?}", e)),
    }
}

/// Main function to start copy trading
pub async fn start_copy_trading(config: CopyTradingConfig) -> Result<(), String> {
    let logger = Logger::new("[COPY-TRADING] => ".green().bold().to_string());
    
    // Initialize global state
    init_global_state();
    
    // Initialize
    logger.log("Initializing copy trading bot...".green().to_string());
    logger.log(format!("Target addresses: {:?}", config.target_addresses));
    logger.log(format!("Protocol preference: {:?}", config.protocol_preference));
    // logger.log(format!("Buy counter limit: {}", config.counter_limit).cyan().to_string()); // Removed
    
    // Log initial counter status
    log_trading_stats(&logger); // Changed to log_trading_stats
    
    // Connect to Yellowstone gRPC
    logger.log("Connecting to Yellowstone gRPC...".green().to_string());
    let mut client = GeyserGrpcClient::build_from_shared(config.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(config.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    // Set up subscribe
    let mut retry_count = 0;
    const MAX_RETRIES: u32 = 3;
    let (subscribe_tx, mut stream) = loop {
        match client.subscribe().await {
            Ok(pair) => break pair,
            Err(e) => {
                retry_count += 1;
                if retry_count >= MAX_RETRIES {
                    return Err(format!("Failed to subscribe after {} attempts: {}", MAX_RETRIES, e));
                }
                logger.log(format!(
                    "[CONNECTION ERROR] => Failed to subscribe (attempt {}/{}): {}. Retrying in 5 seconds...",
                    retry_count, MAX_RETRIES, e
                ).red().to_string());
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    };

    // Convert to Arc to allow cloning across tasks
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));
    // Enable buying
    BUYING_ENABLED.insert((), true);

    // Create config for subscription
    let target_addresses = config.target_addresses.clone();
    // Set excluded addresses to empty array to listen to all transactions
    let excluded_addresses: Vec<String> = vec![];
    
    // Set up subscription
    logger.log("Setting up subscription...".green().to_string());
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "All".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false), // Exclude vote transactions
                failed: Some(false), // Exclude failed transactions
                signature: None,
                account_include: target_addresses.clone(), // Only include transactions involving our targets
                account_exclude: excluded_addresses, // Exclude some common programs
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };
    
    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;
    
    // Create Arc config for tasks
    let config = Arc::new(config);

    // Spawn heartbeat task
    let subscribe_tx_clone = subscribe_tx.clone();
    tokio::spawn(async move {
        loop {
            time::sleep(Duration::from_secs(30)).await;
            if let Err(e) = send_heartbeat_ping(&subscribe_tx_clone).await {
                eprintln!("Heartbeat ping failed: {}", e);
            }
        }
    });

    // Create parallel transaction processor (must be created before spawning monitoring tasks)
    let parallel_processor = ParallelTransactionProcessor::new(
        Arc::new(config.app_state.clone()),
        Arc::new(config.swap_config.clone()),
        config.transaction_landing_mode.clone(),
    );

    // Spawn counter status logging task
    let logger_clone = logger.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        loop {
            time::sleep(Duration::from_secs(60)).await;
            log_trading_stats(&logger_clone); // Changed to log_trading_stats
        }
    });

    // Spawn parallel processor results monitoring task
    let logger_results = logger.clone();
    let parallel_processor_clone = parallel_processor.clone();
    tokio::spawn(async move {
        loop {
            time::sleep(Duration::from_secs(10)).await; // Check every 10 seconds
            
            let completed_operations = parallel_processor_clone.check_completed_operations().await;
            for result in completed_operations {
                if result.success {
                    logger_results.log(format!(
                        "🎉 Parallel operation completed successfully: {:?} in {:?} - Signature: {:?}",
                        result.operation, result.processing_time, result.signature
                    ).green().bold().to_string());
                } else {
                    logger_results.log(format!(
                        "⚠️ Parallel operation failed: {:?} in {:?} - Error: {:?}",
                        result.operation, result.processing_time, result.error
                    ).red().to_string());
                }
            }
        }
    });

    // Start token monitoring service
    let token_monitor = TokenMonitor::new(Arc::new(config.app_state.clone()));
    let token_monitor_clone = token_monitor.clone();
    tokio::spawn(async move {
        if let Err(e) = token_monitor_clone.start_monitoring().await {
            eprintln!("Token monitoring service failed: {}", e);
        }
    });

    // Process incoming messages
    logger.log("Starting to process transactions...".green().to_string());
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(msg) => {
                if let Err(e) = process_message(&msg, config.clone(), &logger, &parallel_processor).await {
                    logger.log(format!("Error processing message: {}", e).red().to_string());
                }
            },
            Err(e) => {
                logger.log(format!("Stream error: {}", e).red().to_string());
                break;
            }
        }
    }

    Err("Stream ended unexpectedly".to_string())
}

/// Process incoming stream messages
async fn process_message(
    msg: &SubscribeUpdate,
    config: Arc<CopyTradingConfig>,
    logger: &Logger,
    parallel_processor: &ParallelTransactionProcessor,
) -> Result<(), String> {
    let start_time = Instant::now();
    
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    
    let mut target_signature = None;
    
    // Helper: extract signer pubkeys from a Yellowstone transaction
    fn extract_signers_from_txn(txn: &SubscribeUpdateTransaction) -> Vec<String> {
        // Best-effort extraction using message header to determine signer count
        let mut result: Vec<String> = Vec::new();
        if let Some(txn_inner) = &txn.transaction {
            if let Some(inner_tx) = &txn_inner.transaction {
                if let Some(message) = &inner_tx.message {
                    let signer_count: usize = message
                        .header
                        .as_ref()
                        .map(|h| h.num_required_signatures as usize)
                        .unwrap_or(1);
                    let take = signer_count.min(message.account_keys.len());
                    for i in 0..take {
                        // Convert account key bytes to base58 string
                        let key_b58 = bs58::encode(&message.account_keys[i]).into_string();
                        result.push(key_b58);
                    }
                }
            }
        }
        result
    }

    // Handle transaction messages
    if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
        // Extract transaction logs and account keys
        if let Some(transaction) = &txn.transaction {
            target_signature = match Signature::try_from(transaction.signature.clone()) {
                Ok(signature) => Some(signature),
                Err(e) => {
                    logger.log(format!("Invalid signature: {:?}", e).red().to_string());
                    return Err(format!("Invalid signature: {:?}", e));
                }
            };
        }
        
        let inner_instructions = match &txn.transaction {
            Some(txn_info) => match &txn_info.meta {
                Some(meta) => meta.inner_instructions.clone(),
                None => vec![],
            },
            None => vec![],
        };
        
        if !inner_instructions.is_empty() {
            // Compute signers (fee payer and any other signers)
            let signers: Vec<String> = extract_signers_from_txn(txn);
            // Find inner instruction with data length of 368, 270, 266, or 146 (Raydium Launchpad)
            let cpi_log_data = inner_instructions
                .iter() 
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 270 || ix.data.len() == 155)
                // .find(|ix| ix.data.len() == 368 || ix.data.len() == 270 || ix.data.len() == 266 || ix.data.len() == 155)
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let logger = logger.clone();
                let txn = txn.clone();
                let parallel_processor = parallel_processor.clone();
                let signers_clone = signers.clone();
                
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::engine::transaction_parser::parse_transaction_data(&txn, &data) {
                        let _ = handle_parsed_data(parsed_data, config, target_signature, &logger, &parallel_processor, signers_clone).await;
                    }
                });
            }
        }
    }
    
    logger.log(format!("Processing overall grpc stream time: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())  
}

/// Handle parsed transaction data for both buying and selling
async fn handle_parsed_data(
    parsed_data: transaction_parser::TradeInfoFromToken,
    config: Arc<CopyTradingConfig>,
    target_signature: Option<Signature>,
    logger: &Logger,
    parallel_processor: &ParallelTransactionProcessor,
    signers: Vec<String>,
) -> Result<(), String> {
    let start_time = Instant::now();
    let instruction_type = parsed_data.dex_type.clone();
    let mint = parsed_data.mint.clone();
    if mint == "So11111111111111111111111111111111111111112" {
        return Ok(());
    }
    // Log current counter status before processing
    let (current_bought, current_sold) = get_trading_stats(); // Changed to get_trading_stats
    logger.log(format!("Processing transaction - Current Bought: {}, Sold: {}", 
        current_bought, current_sold).blue().to_string());
    
    // Log the parsed transaction data
    logger.log(format!(
        "Token transaction detected for {}: Instruction: {}, Is buy: {}",
        mint,
        match instruction_type {
            transaction_parser::DexType::PumpSwap => "PumpSwap",
            transaction_parser::DexType::PumpFun => "PumpFun",
            transaction_parser::DexType::RaydiumLaunchpad => "RaydiumLaunchpad",
            _ => "Unknown",
        },
        parsed_data.is_buy
    ).green().to_string());
    
    // Determine protocol to use
    let protocol = match instruction_type {
        transaction_parser::DexType::PumpSwap => SwapProtocol::PumpSwap,
        transaction_parser::DexType::PumpFun => SwapProtocol::PumpFun,
        transaction_parser::DexType::RaydiumLaunchpad => SwapProtocol::RaydiumLaunchpad,
        _ => config.protocol_preference.clone(),
    };
    
    // Handle buy/sell based on transaction type
    if parsed_data.is_buy {
        // Target is buying - we should buy too
        logger.log(format!("Target is BUYING token: {}", mint).green().to_string());
        
        // Check if buying is enabled
        let buying_enabled = BUYING_ENABLED.get(&()).map(|ref_val| *ref_val).unwrap_or(true);
        if !buying_enabled {
            logger.log("Buying is currently disabled".yellow().to_string());
            return Ok(());
        }
        
        // Determine which target address triggered this trade (first matching signer)
        let maybe_target = signers.iter().find(|s| config.target_addresses.iter().any(|t| t == *s)).cloned();
        if maybe_target.is_none() {
            logger.log(format!("No matching target signer found for BUY {}, signers: {:?}", mint, signers).yellow().to_string());
            // return Ok(());  // enable this to skip buy if no matching signer 
        }
        let target_addr = maybe_target.unwrap();

        // Special target wallet handling: one-time-per-mint buy
        let is_special = {
            let set = SPECIAL_TARGET_WALLETS.read().unwrap();
            set.contains(&target_addr)
        };
        if is_special {
            // Check and record
            let mut bought_map = SPECIAL_TARGET_WALLET_BOUGHT.write().unwrap();
            let entry = bought_map.entry(target_addr.clone()).or_insert_with(HashSet::new);
            if entry.contains(&mint) {
                logger.log(format!("Special target {} already bought {}, skipping buy", target_addr, mint).yellow().to_string());
                return Ok(());
            } else {
                entry.insert(mint.clone());
            }
        }

        // Compute target amounts and our copy amounts
        let target_sol = parsed_data.sol_change.abs();
        let target_tokens = parsed_data.token_change.abs();
        // Fetch copy rate percent for this address
        let copy_rate_percent = {
            let map = PER_ADDRESS_COPY_RATE.read().unwrap();
            *map.get(&target_addr).unwrap_or(&10.0)
        };
        let bot_buy_sol = target_sol * (copy_rate_percent / 100.0);
        let bot_buy_tokens = target_tokens * (copy_rate_percent / 100.0);

        // Record per-address positions in cache
        {
            let mut positions = COPY_POSITIONS.write().unwrap();
            let entry = positions.entry(target_addr.clone()).or_insert_with(HashMap::new);
            let mint_entry = entry.entry(mint.clone()).or_insert_with(CopyPosition::default);
            mint_entry.add_buy(target_tokens, bot_buy_tokens);
        }

        // Queue dynamic buy SOL amount for this mint (consumed by selling engine)
        {
            let mut buys = UPCOMING_BUY_SOL.write().unwrap();
            let val = buys.entry(mint.clone()).or_insert(0.0);
            *val += bot_buy_sol;
        }

        // Record hashmap (target_address, target_buy_amount, bot_buy_amount)
        {
            let mut records = crate::common::cache::COPY_BUY_RECORDS.write().unwrap();
            let list = records.entry(mint.clone()).or_insert_with(Vec::new);
            list.push((target_addr.clone(), target_sol, bot_buy_sol));
        }

        // Submit buy operation to parallel processor (non-blocking)
        logger.log(format!("🚀 Submitting BUY operation to parallel processor for token: {} (copy_rate: {}%, bot_buy_sol: {:.9})", mint, copy_rate_percent, bot_buy_sol).cyan().to_string());
        
        match parallel_processor.submit_buy_operation(parsed_data.clone()) {
            Ok(_) => {
                logger.log(format!("✅ BUY operation submitted successfully for token: {}", mint).green().to_string());
                
                // Update bought counter (optimistic - the actual execution happens in background)
                if let Some(mut counter) = BOUGHT_COUNT.get_mut(&()) {
                    *counter += 1;
                }
                LAST_BUY_TIME.insert((), Some(Instant::now()));
            },
            Err(e) => {
                logger.log(format!("❌ Failed to submit BUY operation for token {}: {}", mint, e).red().to_string());
                return Err(format!("Failed to submit buy operation: {}", e));
            }
        }
    } else {
        // For sells, require that a signer matches one of the configured target addresses
        let maybe_target = signers.iter().find(|s| config.target_addresses.iter().any(|t| t == *s)).cloned();
        if maybe_target.is_none() {
            logger.log(format!(
                "Skipping SELL for {}: signer(s) {:?} do not match target addresses",
                mint, signers
            ).yellow().to_string());
            return Ok(());
        }
        let target_addr = maybe_target.unwrap();

        // Compute proportional bot sell tokens based on tracked positions
        let target_sell_tokens = parsed_data.token_change.abs();
        let bot_sell_tokens = {
            let mut positions = COPY_POSITIONS.write().unwrap();
            if let Some(mints) = positions.get_mut(&target_addr) {
                if let Some(pos) = mints.get_mut(&mint) {
                    pos.apply_target_sell_and_compute_bot_sell(target_sell_tokens)
                } else { 0.0 }
            } else { 0.0 }
        };

        if bot_sell_tokens > 0.0 {
            // Queue dynamic sell token amount for this mint (consumed by selling engine)
            let mut sells = UPCOMING_SELL_TOKENS.write().unwrap();
            let val = sells.entry(mint.clone()).or_insert(0.0);
            *val += bot_sell_tokens;
            logger.log(format!("Proportional SELL queued for {} tokens on mint {}", bot_sell_tokens, mint).red().to_string());
        } else {
            logger.log(format!("No proportional sell computed for mint {} (no position)", mint).yellow().to_string());
        }

        // Submit sell operation to parallel processor (non-blocking with must-selling)
        logger.log(format!("🚀 Submitting SELL operation to parallel processor for token: {}", mint).red().to_string());
        
        match parallel_processor.submit_sell_operation(parsed_data.clone()) {
            Ok(_) => {
                logger.log(format!("✅ SELL operation submitted successfully for token: {}", mint).green().to_string());
                
                // Update sold counter (optimistic - the actual execution happens in background with retries)
                if let Some(mut counter) = SOLD_COUNT.get_mut(&()) {
                    *counter += 1;
                }
            },
            Err(e) => {
                logger.log(format!("❌ Failed to submit SELL operation for token {}: {}", mint, e).red().to_string());
                return Err(format!("Failed to submit sell operation: {}", e));
            }
        }
    }
    
    logger.log(format!("Transaction processing time: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())
} 
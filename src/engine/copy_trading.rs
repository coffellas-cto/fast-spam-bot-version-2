use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use anyhow::Result;
use anchor_client::solana_sdk::signature::Signature;
use colored::Colorize;
use tokio::time;
use futures_util::stream::StreamExt;
use futures_util::{SinkExt, Sink};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
    SubscribeRequestFilterTransactions,  SubscribeUpdate,
};
use crate::engine::transaction_parser;
use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
};
use crate::engine::swap::SwapProtocol;
use crate::engine::selling_strategy::SimpleSellingEngine;
use crate::services::token_monitor::TokenMonitor;
use dashmap::DashMap;

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

    // Spawn counter status logging task
    let logger_clone = logger.clone();
    let config_clone = config.clone();
    tokio::spawn(async move {
        loop {
            time::sleep(Duration::from_secs(60)).await;
            log_trading_stats(&logger_clone); // Changed to log_trading_stats
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

    // Create simple selling engine
    let selling_engine = SimpleSellingEngine::new(
        Arc::new(config.app_state.clone()),
        Arc::new(config.swap_config.clone()),
        config.transaction_landing_mode.clone(),
    );

    // Process incoming messages
    logger.log("Starting to process transactions...".green().to_string());
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(msg) => {
                if let Err(e) = process_message(&msg, config.clone(), &logger, &selling_engine).await {
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
    selling_engine: &SimpleSellingEngine,
) -> Result<(), String> {
    let start_time = Instant::now();
    
    // Handle ping messages
    if let Some(UpdateOneof::Ping(_ping)) = &msg.update_oneof {
        return Ok(());
    }
    
    let mut target_signature = None;
    
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
            // Find inner instruction with data length of 368, 270, 266, or 146 (Raydium Launchpad)
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 270 || ix.data.len() == 266 || ix.data.len() == 146)
                .map(|ix| ix.data.clone());

            if let Some(data) = cpi_log_data {
                let config = config.clone();
                let logger = logger.clone();
                let txn = txn.clone();
                let selling_engine = selling_engine.clone();
                
                tokio::spawn(async move {
                    if let Some(parsed_data) = crate::engine::transaction_parser::parse_transaction_data(&txn, &data) {
                        let _ = handle_parsed_data(parsed_data, config, target_signature, &logger, &selling_engine).await;
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
    selling_engine: &SimpleSellingEngine,
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
        
        // Execute buy
        logger.log(format!("Proceeding with BUY for token: {}", mint).cyan().to_string());
        
        match selling_engine.execute_buy(&parsed_data).await {
            Ok(_) => {
                logger.log(format!("Successfully executed BUY for token: {}", mint).green().to_string());
                
                // Update bought counter
                if let Some(mut counter) = BOUGHT_COUNT.get_mut(&()) { // Changed to BOUGHT_COUNT
                    *counter += 1;
                }
                LAST_BUY_TIME.insert((), Some(Instant::now()));
                
            },
            Err(e) => {
                logger.log(format!("Failed to execute BUY for token {}: {}", mint, e).red().to_string());
                
                return Err(format!("Failed to execute buy: {}", e));
            }
        }
    } else {
        // Target is selling - we should sell too
        logger.log(format!("Target is SELLING token: {}", mint).red().to_string());
        
        // Execute sell
        match selling_engine.execute_sell(&parsed_data).await {
            Ok(_) => {
                logger.log(format!("Successfully executed SELL for token: {}", mint).green().to_string());
                
                // Update sold counter
                if let Some(mut counter) = SOLD_COUNT.get_mut(&()) { // Changed to SOLD_COUNT
                    *counter += 1;
                }
                
            },
            Err(e) => {
                logger.log(format!("Failed to execute SELL for token {}: {}", mint, e).red().to_string());
                
                return Err(format!("Failed to execute sell: {}", e));
            }
        }
    }
    
    logger.log(format!("Transaction processing time: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())
} 
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
    config::{AppState, SwapConfig, JUPITER_PROGRAM, OKX_DEX_PROGRAM},
    logger::Logger,
};
use crate::engine::swap::SwapProtocol;
use crate::engine::selling_strategy::SimpleSellingEngine;
use crate::services::token_monitor::TokenMonitor;
use dashmap::DashMap;

// Global state for copy trading
lazy_static::lazy_static! {
    static ref COUNTER: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref SOLD_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref BOUGHT_TOKENS: Arc<DashMap<(), u64>> = Arc::new(DashMap::new());
    static ref LAST_BUY_TIME: Arc<DashMap<(), Option<Instant>>> = Arc::new(DashMap::new());
    static ref BUYING_ENABLED: Arc<DashMap<(), bool>> = Arc::new(DashMap::new());
}

// Initialize the global counters with default values
fn init_global_state() {
    COUNTER.insert((), 0);
    SOLD_TOKENS.insert((), 0);
    BOUGHT_TOKENS.insert((), 0);
    LAST_BUY_TIME.insert((), None);
    BUYING_ENABLED.insert((), true);
}

/// Get current counter status
fn get_counter_status() -> (u64, u64, u64) {
    let counter = COUNTER.get(&()).map(|ref_val| *ref_val).unwrap_or(0);
    let bought = BOUGHT_TOKENS.get(&()).map(|ref_val| *ref_val).unwrap_or(0);
    let sold = SOLD_TOKENS.get(&()).map(|ref_val| *ref_val).unwrap_or(0);
    (counter, bought, sold)
}

/// Log current counter status
fn log_counter_status(logger: &Logger, counter_limit: u64) {
    let (counter, bought, sold) = get_counter_status();
    logger.log(format!("Counter Status - Buy Counter: {}/{}, Bought: {}, Sold: {}", 
        counter, counter_limit, bought, sold).blue().to_string());
}

/// Reset counter (useful for manual intervention)
pub fn reset_counter() {
    COUNTER.insert((), 0);
    SOLD_TOKENS.insert((), 0);
    BOUGHT_TOKENS.insert((), 0);
    println!("Counter reset successfully");
}

/// Get current counter status (public function)
pub fn get_current_counter_status() -> (u64, u64, u64) {
    get_counter_status()
}

/// Configuration for copy trading
pub struct CopyTradingConfig {
    pub yellowstone_grpc_http: String,
    pub yellowstone_grpc_token: String,
    pub app_state: AppState,
    pub swap_config: SwapConfig,
    pub counter_limit: u64,
    pub target_addresses: Vec<String>,
    pub excluded_addresses: Vec<String>,
    pub protocol_preference: SwapProtocol,
    pub is_progressive_sell: bool,
    pub is_copy_selling: bool,
    pub is_reverse: bool,
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
    logger.log(format!("Copy selling enabled: {}", config.is_copy_selling).cyan().to_string());
    logger.log(format!("Reverse mode enabled: {}", config.is_reverse).cyan().to_string());
    logger.log(format!("Buy counter limit: {}", config.counter_limit).cyan().to_string());
    
    // Log initial counter status
    log_counter_status(&logger, config.counter_limit);
    
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
    // Add excluded addresses
    let mut excluded_addresses = vec![JUPITER_PROGRAM.to_string(), OKX_DEX_PROGRAM.to_string()];
    excluded_addresses.extend(config.excluded_addresses.clone());
    
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
            log_counter_status(&logger_clone, config_clone.counter_limit);
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
            // Find inner instruction with data length of 368 or 233
            let cpi_log_data = inner_instructions
                .iter()
                .flat_map(|inner| &inner.instructions)
                .find(|ix| ix.data.len() == 368 || ix.data.len() == 233)
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
    
    // Log current counter status before processing
    let (current_counter, bought_count, sold_count) = get_counter_status();
    logger.log(format!("Processing transaction - Current Counter: {}/{}, Bought: {}, Sold: {}", 
        current_counter, config.counter_limit, bought_count, sold_count).blue().to_string());
    
    // Log the parsed transaction data
    logger.log(format!(
        "Token transaction detected for {}: Instruction: {}, Is buy: {}",
        mint,
        match instruction_type {
            transaction_parser::DexType::PumpSwap => "PumpSwap",
            transaction_parser::DexType::PumpFun => "PumpFun",
            _ => "Unknown",
        },
        parsed_data.is_buy
    ).green().to_string());
    
    // Determine protocol to use
    let protocol = match instruction_type {
        transaction_parser::DexType::PumpSwap => SwapProtocol::PumpSwap,
        transaction_parser::DexType::PumpFun => SwapProtocol::PumpFun,
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
        
        // Check counter limit before buying
        if current_counter >= config.counter_limit {
            logger.log(format!("Buy counter limit reached ({}/{}), skipping buy", current_counter, config.counter_limit).red().to_string());
            return Ok(());
        }
        
        // Execute buy
        match selling_engine.execute_buy(&parsed_data).await {
            Ok(_) => {
                logger.log(format!("Successfully executed BUY for token: {}", mint).green().to_string());
                
                // Update counters
                if let Some(mut counter) = COUNTER.get_mut(&()) {
                    *counter += 1;
                    logger.log(format!("Buy counter increased to {}/{}", *counter, config.counter_limit).cyan().to_string());
                }
                if let Some(mut counter) = BOUGHT_TOKENS.get_mut(&()) {
                    *counter += 1;
                }
                LAST_BUY_TIME.insert((), Some(Instant::now()));
                
                // Log token tracking status
                let tracking_count = crate::common::cache::BOUGHT_TOKENS.size();
                logger.log(format!("Now tracking {} tokens", tracking_count).blue().to_string());
                
                // Send notification
                if let Err(e) = crate::services::telegram::send_trade_notification(
                    &parsed_data,
                    &format!("{:?}", protocol),
                    "BUYING"
                ).await {
                    logger.log(format!("Failed to send Telegram notification: {}", e).red().to_string());
                }
            },
            Err(e) => {
                logger.log(format!("Failed to execute BUY for token {}: {}", mint, e).red().to_string());
                
                // Send error notification
                if let Err(te) = crate::services::telegram::send_error_notification(
                    &format!("Buy error for {}: {}", mint, e)
                ).await {
                    logger.log(format!("Failed to send Telegram notification: {}", te).red().to_string());
                }
                
                return Err(format!("Failed to execute buy: {}", e));
            }
        }
    } else {
        // Target is selling - we should sell too (if copy selling is enabled)
        if config.is_copy_selling {
            logger.log(format!("Target is SELLING token: {}", mint).red().to_string());
            
            // Always decrease counter when target sells, even if we don't own the token
            // This maintains proper counter balance
            if let Some(mut counter) = COUNTER.get_mut(&()) {
                if *counter > 0 {
                    *counter -= 1;
                    logger.log(format!("Buy counter decreased to {}/{} (target sold)", *counter, config.counter_limit).cyan().to_string());
                }
            }
            
            // Execute sell
            match selling_engine.execute_sell(&parsed_data).await {
                Ok(_) => {
                    logger.log(format!("Successfully executed SELL for token: {}", mint).green().to_string());
                    
                    // Update sold counter
                    if let Some(mut counter) = SOLD_TOKENS.get_mut(&()) {
                        *counter += 1;
                    }
                    
                    // Log token tracking status
                    let tracking_count = crate::common::cache::BOUGHT_TOKENS.size();
                    logger.log(format!("Now tracking {} tokens", tracking_count).blue().to_string());
                    
                    // Send notification
                    if let Err(e) = crate::services::telegram::send_trade_notification(
                        &parsed_data,
                        &format!("{:?}", protocol),
                        "SELLING"
                    ).await {
                        logger.log(format!("Failed to send Telegram notification: {}", e).red().to_string());
                    }
                },
                Err(e) => {
                    logger.log(format!("Failed to execute SELL for token {}: {}", mint, e).red().to_string());
                    
                    // Send error notification
                    if let Err(te) = crate::services::telegram::send_error_notification(
                        &format!("Sell error for {}: {}", mint, e)
                    ).await {
                        logger.log(format!("Failed to send Telegram notification: {}", te).red().to_string());
                    }
                    
                    return Err(format!("Failed to execute sell: {}", e));
                }
            }
        } else {
            logger.log("Copy selling is disabled, ignoring sell transaction".yellow().to_string());
        }
    }
    
    logger.log(format!("Transaction processing time: {:?}", start_time.elapsed()).blue().to_string());
    Ok(())
} 
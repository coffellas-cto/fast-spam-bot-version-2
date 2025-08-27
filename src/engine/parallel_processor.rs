use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use colored::Colorize;
use tokio::sync::mpsc;
use tokio::time::Instant;
use solana_sdk::signature::Signer;
use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
};
use crate::engine::transaction_parser::TradeInfoFromToken;
use crate::engine::selling_strategy::SimpleSellingEngine;
use crate::services::{jupiter::JupiterClient, balance_manager::BalanceManager};

/// Transaction operation type for parallel processing
#[derive(Debug, Clone)]
pub enum TransactionOperation {
    Buy(TradeInfoFromToken),
    Sell(TradeInfoFromToken),
}

/// Result of parallel transaction processing
#[derive(Debug)]
pub struct ProcessingResult {
    pub operation: TransactionOperation,
    pub success: bool,
    pub signature: Option<String>,
    pub error: Option<String>,
    pub processing_time: Duration,
}

/// Parallel transaction processor that handles buy/sell operations in separate spawned tasks
/// This ensures the main monitoring stream is never blocked by transaction processing
pub struct ParallelTransactionProcessor {
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    transaction_landing_mode: crate::common::config::TransactionLandingMode,
    selling_engine: SimpleSellingEngine,
    jupiter_client: JupiterClient,
    balance_manager: BalanceManager,
    logger: Logger,
    // Channels for communication
    operation_sender: mpsc::UnboundedSender<TransactionOperation>,
    result_receiver: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<ProcessingResult>>>,
}

impl ParallelTransactionProcessor {
    /// Create a new parallel transaction processor
    pub fn new(
        app_state: Arc<AppState>,
        swap_config: Arc<SwapConfig>,
        transaction_landing_mode: crate::common::config::TransactionLandingMode,
    ) -> Self {
        let selling_engine = SimpleSellingEngine::new(
            app_state.clone(),
            swap_config.clone(),
            transaction_landing_mode.clone(),
        );
        
        let jupiter_client = JupiterClient::new(app_state.rpc_nonblocking_client.clone());
        let balance_manager = BalanceManager::new(app_state.clone());
        
        // Create channels for communication
        let (operation_sender, operation_receiver) = mpsc::unbounded_channel();
        let (result_sender, result_receiver) = mpsc::unbounded_channel();
        
        let processor = Self {
            app_state: app_state.clone(),
            swap_config,
            transaction_landing_mode,
            selling_engine: selling_engine.clone(),
            jupiter_client: jupiter_client.clone(),
            balance_manager: balance_manager.clone(),
            logger: Logger::new("[PARALLEL-PROCESSOR] => ".magenta().to_string()),
            operation_sender,
            result_receiver: Arc::new(tokio::sync::Mutex::new(result_receiver)),
        };
        
        // Start the background processing task
        processor.start_background_processor(operation_receiver, result_sender);
        
        processor
    }
    
    /// Start the background task that processes operations
    fn start_background_processor(
        &self,
        mut operation_receiver: mpsc::UnboundedReceiver<TransactionOperation>,
        result_sender: mpsc::UnboundedSender<ProcessingResult>,
    ) {
        let selling_engine = self.selling_engine.clone();
        let jupiter_client = self.jupiter_client.clone();
        let balance_manager = self.balance_manager.clone();
        let logger = self.logger.clone();
        
        tokio::spawn(async move {
            logger.log("🚀 Parallel transaction processor started".green().bold().to_string());
            
            while let Some(operation) = operation_receiver.recv().await {
                let start_time = Instant::now();
                let selling_engine = selling_engine.clone();
                let jupiter_client = jupiter_client.clone();
                let balance_manager = balance_manager.clone();
                let result_sender = result_sender.clone();
                let logger = logger.clone();
                
                // Spawn each operation in its own task for maximum parallelism
                tokio::spawn(async move {
                    let result = match operation.clone() {
                        TransactionOperation::Buy(trade_info) => {
                            Self::process_buy_operation(selling_engine, trade_info, &logger).await
                        },
                        TransactionOperation::Sell(trade_info) => {
                            Self::process_sell_operation_with_must_selling(
                                selling_engine, 
                                jupiter_client, 
                                balance_manager, 
                                trade_info, 
                                &logger
                            ).await
                        },
                    };
                    
                    let processing_result = ProcessingResult {
                        operation,
                        success: result.is_ok(),
                        signature: result.as_ref().ok().cloned(),
                        error: result.as_ref().err().map(|e| e.to_string()),
                        processing_time: start_time.elapsed(),
                    };
                    
                    // Send result back (non-blocking)
                    let _ = result_sender.send(processing_result);
                });
            }
            
            logger.log("⚠️ Parallel transaction processor stopped - operation channel closed".yellow().to_string());
        });
    }
    
    /// Submit a buy operation for parallel processing (non-blocking)
    pub fn submit_buy_operation(&self, trade_info: TradeInfoFromToken) -> Result<()> {
        self.logger.log(format!("📤 Submitting BUY operation for token: {}", trade_info.mint).cyan().to_string());
        
        self.operation_sender.send(TransactionOperation::Buy(trade_info))
            .map_err(|e| anyhow::anyhow!("Failed to submit buy operation: {}", e))?;
        
        Ok(())
    }
    
    /// Submit a sell operation for parallel processing (non-blocking)
    pub fn submit_sell_operation(&self, trade_info: TradeInfoFromToken) -> Result<()> {
        self.logger.log(format!("📤 Submitting SELL operation for token: {}", trade_info.mint).red().to_string());
        
        self.operation_sender.send(TransactionOperation::Sell(trade_info))
            .map_err(|e| anyhow::anyhow!("Failed to submit sell operation: {}", e))?;
        
        Ok(())
    }
    
    /// Check for completed operations (non-blocking)
    pub async fn check_completed_operations(&self) -> Vec<ProcessingResult> {
        let mut results = Vec::new();
        let mut receiver = self.result_receiver.lock().await;
        
        // Collect all available results without blocking
        while let Ok(result) = receiver.try_recv() {
            results.push(result);
        }
        
        results
    }
    
    /// Process buy operation in background task
    async fn process_buy_operation(
        selling_engine: SimpleSellingEngine,
        trade_info: TradeInfoFromToken,
        logger: &Logger,
    ) -> Result<String> {
        logger.log(format!("🔄 Processing BUY operation for token: {}", trade_info.mint).green().to_string());
        
        match selling_engine.execute_buy(&trade_info).await {
            Ok(_) => {
                logger.log(format!("✅ BUY operation completed for token: {}", trade_info.mint).green().bold().to_string());
                Ok(format!("BUY completed for {}", trade_info.mint))
            },
            Err(e) => {
                logger.log(format!("❌ BUY operation failed for token {}: {}", trade_info.mint, e).red().to_string());
                Err(anyhow::anyhow!("BUY failed: {}", e))
            }
        }
    }
    
    /// Process sell operation with must-selling logic (multiple fallbacks)
    async fn process_sell_operation_with_must_selling(
        selling_engine: SimpleSellingEngine,
        jupiter_client: JupiterClient,
        balance_manager: BalanceManager,
        trade_info: TradeInfoFromToken,
        logger: &Logger,
    ) -> Result<String> {
        logger.log(format!("🔄 Processing SELL operation with must-selling for token: {}", trade_info.mint).red().to_string());
        
        const MAX_RETRIES: u32 = 3;
        let mut last_error: Option<anyhow::Error> = None;
        
        // Strategy 1: Try native DEX selling first (fastest)
        for attempt in 1..=MAX_RETRIES {
            logger.log(format!("🎯 SELL attempt {}/{} using native DEX for token: {}", attempt, MAX_RETRIES, trade_info.mint).cyan().to_string());
            
            match selling_engine.execute_sell(&trade_info).await {
                Ok(_) => {
                    logger.log(format!("✅ Native DEX sell completed for token: {} (attempt {})", trade_info.mint, attempt).green().bold().to_string());
                    return Ok(format!("Native DEX sell completed for {}", trade_info.mint));
                },
                Err(e) => {
                    logger.log(format!("⚠️ Native DEX sell attempt {}/{} failed for token {}: {}", attempt, MAX_RETRIES, trade_info.mint, e).yellow().to_string());
                    last_error = Some(e);
                    
                    // Wait before retry (except on last attempt)
                    if attempt < MAX_RETRIES {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
        
        logger.log(format!("⚠️ All native DEX sell attempts failed for token: {}, trying Jupiter fallback", trade_info.mint).yellow().to_string());
        
        // Strategy 2: Jupiter API fallback (check token balance and sell using Jupiter)
        match Self::attempt_jupiter_fallback_sell(&selling_engine, &jupiter_client, &trade_info, logger).await {
            Ok(signature) => {
                logger.log(format!("✅ Jupiter fallback sell completed for token: {}", trade_info.mint).green().bold().to_string());
                
                // Trigger balance management after successful Jupiter sell
                if let Err(e) = balance_manager.manage_balances_after_selling().await {
                    logger.log(format!("⚠️ Balance management failed after Jupiter sell: {}", e).yellow().to_string());
                }
                
                return Ok(signature);
            },
            Err(jupiter_error) => {
                logger.log(format!("❌ Jupiter fallback also failed for token {}: {}", trade_info.mint, jupiter_error).red().to_string());
                last_error = Some(jupiter_error);
            }
        }
        
        // If all strategies fail, return the last error
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All sell strategies failed for token {}", trade_info.mint)))
    }
    
    /// Attempt Jupiter fallback selling
    async fn attempt_jupiter_fallback_sell(
        selling_engine: &SimpleSellingEngine,
        jupiter_client: &JupiterClient,
        trade_info: &TradeInfoFromToken,
        logger: &Logger,
    ) -> Result<String> {
        logger.log(format!("🪐 Attempting Jupiter fallback sell for token: {}", trade_info.mint).magenta().to_string());
        
        // Check if we have any balance of this token
        use std::str::FromStr;
        use anchor_client::solana_sdk::pubkey::Pubkey;
        use spl_associated_token_account::get_associated_token_address;
        
        let wallet_pubkey = selling_engine.app_state().wallet.try_pubkey()
            .map_err(|e| anyhow::anyhow!("Failed to get wallet pubkey: {}", e))?;
        
        let token_pubkey = Pubkey::from_str(&trade_info.mint)
            .map_err(|e| anyhow::anyhow!("Invalid token mint: {}", e))?;
        
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
        
        // Get token balance
        match selling_engine.app_state().rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                let amount_raw = account.token_amount.amount.parse::<u64>()
                    .map_err(|e| anyhow::anyhow!("Failed to parse token amount: {}", e))?;
                
                if amount_raw == 0 {
                    logger.log(format!("ℹ️ No balance found for token {}, skipping Jupiter sell", trade_info.mint).yellow().to_string());
                    return Ok(format!("No balance for {}", trade_info.mint));
                }
                
                let decimals = account.token_amount.decimals;
                logger.log(format!("💰 Found balance for token {}: {} raw units", trade_info.mint, amount_raw).green().to_string());
                
                // Perform Jupiter sell with retries
                const JUPITER_RETRIES: u32 = 3;
                for attempt in 1..=JUPITER_RETRIES {
                    logger.log(format!("🪐 Jupiter sell attempt {}/{} for token: {}", attempt, JUPITER_RETRIES, trade_info.mint).blue().to_string());
                    
                    match jupiter_client.sell_token_with_jupiter(
                        &trade_info.mint,
                        amount_raw,
                        100, // 1% slippage
                        &selling_engine.app_state().wallet
                    ).await {
                        Ok(signature) => {
                            logger.log(format!("✅ Jupiter sell successful (attempt {}): {}", attempt, signature).green().bold().to_string());
                            return Ok(signature);
                        },
                        Err(e) => {
                            logger.log(format!("⚠️ Jupiter sell attempt {}/{} failed: {}", attempt, JUPITER_RETRIES, e).yellow().to_string());
                            
                            if attempt < JUPITER_RETRIES {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                        }
                    }
                }
                
                Err(anyhow::anyhow!("All Jupiter sell attempts failed"))
            },
            Ok(None) => {
                logger.log(format!("ℹ️ No token account found for {}, nothing to sell", trade_info.mint).yellow().to_string());
                Ok(format!("No token account for {}", trade_info.mint))
            },
            Err(e) => {
                Err(anyhow::anyhow!("Failed to check token balance: {}", e))
            }
        }
    }
    
    /// Get processing statistics
    pub async fn get_processing_stats(&self) -> (usize, usize) {
        // For now, return simple stats - in the future we could track more detailed metrics
        (0, 0) // (pending_operations, completed_operations)
    }
}

impl Clone for ParallelTransactionProcessor {
    fn clone(&self) -> Self {
        // Create a new processor with the same configuration
        Self::new(
            self.app_state.clone(),
            self.swap_config.clone(),
            self.transaction_landing_mode.clone(),
        )
    }
}

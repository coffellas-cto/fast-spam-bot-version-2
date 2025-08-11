use std::sync::Arc;
use std::str::FromStr;
use anyhow::{anyhow, Result};
use anchor_client::solana_sdk::{hash::Hash, instruction::Instruction, pubkey::Pubkey, signature::{Keypair, Signature}};
use colored::Colorize;
use spl_associated_token_account::get_associated_token_address;

use crate::common::{
    config::{AppState, SwapConfig},
    logger::Logger,
    cache::{WALLET_TOKEN_ACCOUNTS, BOUGHT_TOKENS},
};
use crate::engine::transaction_parser::{TradeInfoFromToken, DexType};
use crate::engine::swap::{SwapDirection, SwapProtocol};
use crate::dex::pump_fun::Pump;
use crate::dex::pump_swap::PumpSwap;
use crate::services::jupiter::JupiterClient;
use solana_sdk::signature::Signer;
/// Simple selling engine for basic buy/sell operations
#[derive(Clone)]
pub struct SimpleSellingEngine {
    app_state: Arc<AppState>,
    swap_config: Arc<SwapConfig>,
    transaction_landing_mode: crate::common::config::TransactionLandingMode,
    logger: Logger,
}

impl SimpleSellingEngine {
    pub fn new(
        app_state: Arc<AppState>,
        swap_config: Arc<SwapConfig>,
        transaction_landing_mode: crate::common::config::TransactionLandingMode,
    ) -> Self {
        Self {
            app_state,
            swap_config,
            transaction_landing_mode,
            logger: Logger::new("[SIMPLE-SELLING] => ".yellow().to_string()),
        }
    }
    
    /// Execute a buy operation when target buys
    pub async fn execute_buy(&self, trade_info: &TradeInfoFromToken) -> Result<()> {
        self.logger.log(format!("Executing BUY for token: {}", trade_info.mint).green().to_string());
        
        // Create buy config with amount limiting
        let mut buy_config = (*self.swap_config).clone();
        buy_config.swap_direction = SwapDirection::Buy;
        
        // Define max buy amount to prevent BuyMoreBaseAmountThanPoolReserves errors
        const MAX_BUY_AMOUNT: f64 = 1.0; // 1 SOL max buy amount
        
        // Use configured TOKEN_AMOUNT from .env file and apply safety limit
        let configured_amount = buy_config.amount_in; // This contains TOKEN_AMOUNT from .env
        let limited_amount = configured_amount.min(MAX_BUY_AMOUNT);
        
        if limited_amount != configured_amount {
            self.logger.log(format!("Limited buy amount from {} to {} SOL (max_buy_amount)", 
                configured_amount, limited_amount).yellow().to_string());
        }
        
        self.logger.log(format!("Using buy amount: {} SOL (from TOKEN_AMOUNT config)", limited_amount).green().to_string());
        buy_config.amount_in = limited_amount;

        // Execute buy based on protocol
        let result = match self.get_protocol_from_trade_info(trade_info) {
            SwapProtocol::PumpFun => {
                let pump = Pump::new(
                    self.app_state.rpc_nonblocking_client.clone(),
                    self.app_state.rpc_client.clone(),
                    self.app_state.wallet.clone(),
                );
                
                match pump.build_swap_from_parsed_data(trade_info, buy_config).await {
                    Ok((keypair, instructions, price)) => {
                        self.logger.log(format!("Generated PumpFun buy instruction at price: {}", price));
                        
                        // Get recent blockhash
                        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        
                        // Execute with transaction landing mode
                        match crate::core::tx::new_signed_and_send_with_landing_mode(
                            self.transaction_landing_mode.clone(),
                            &self.app_state,
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.logger.log(format!("Buy transaction sent: {}", signature).green().to_string());
                                
                                // Track the bought token
                                self.track_bought_token(trade_info, &signature.to_string(), "PumpFun").await?;
                                
                                Ok(())
                            },
                            Err(e) => {
                                self.logger.log(format!("Buy transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send buy transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build PumpFun buy instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build buy instruction: {}", e))
                    }
                }
            },
            SwapProtocol::PumpSwap => {
                let pump_swap = PumpSwap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                // Try with original amount first, then retry with reduced amounts if needed
                let mut attempt_amount = buy_config.amount_in;
                let mut last_error: Option<anyhow::Error> = None;
                
                for attempt in 1..=3 {
                    self.logger.log(format!("PumpSwap buy attempt {} with amount: {}", attempt, attempt_amount).blue().to_string());
                    
                    let mut retry_config = buy_config.clone();
                    retry_config.amount_in = attempt_amount;
                    
                    match pump_swap.build_swap_from_parsed_data(trade_info, retry_config).await {
                        Ok((keypair, instructions, price)) => {
                            self.logger.log(format!("Generated PumpSwap buy instruction at price: {}", price));
                            
                            // Get recent blockhash
                            let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                                Some(hash) => hash,
                                None => {
                                    self.logger.log("Failed to get recent blockhash".red().to_string());
                                    return Err(anyhow!("Failed to get recent blockhash"));
                                }
                            };
                            
                            // Execute with transaction landing mode
                            match crate::core::tx::new_signed_and_send_with_landing_mode(
                                self.transaction_landing_mode.clone(),
                                &self.app_state,
                                recent_blockhash,
                                &keypair,
                                instructions,
                                &self.logger,
                            ).await {
                                Ok(signatures) => {
                                    if signatures.is_empty() {
                                        return Err(anyhow!("No transaction signature returned"));
                                    }
                                    
                                    let signature = &signatures[0];
                                    self.logger.log(format!("Buy transaction sent: {}", signature).green().to_string());
                                    
                                    // Track the bought token
                                    self.track_bought_token(trade_info, &signature.to_string(), "PumpSwap").await?;
                                    
                                    return Ok(());
                                },
                                Err(e) => {
                                    let error_str = e.to_string();
                                    if error_str.contains("BuyMoreBaseAmountThanPoolReserves") || error_str.contains("0x1780") {
                                        self.logger.log(format!("BuyMoreBaseAmountThanPoolReserves error on attempt {}, reducing amount", attempt).yellow().to_string());
                                        last_error = Some(anyhow!("BuyMoreBaseAmountThanPoolReserves error: {}", e));
                                        
                                        // Reduce amount by 50% for next attempt
                                        attempt_amount *= 0.5;
                                        
                                        if attempt_amount < 0.001 {
                                            self.logger.log("Amount too small, giving up".red().to_string());
                                            break;
                                        }
                                        
                                        continue;
                                    } else {
                                        self.logger.log(format!("Buy transaction failed: {}", e).red().to_string());
                                        return Err(anyhow!("Failed to send buy transaction: {}", e));
                                    }
                                }
                            }
                        },
                        Err(e) => {
                            self.logger.log(format!("Failed to build PumpSwap buy instruction: {}", e).red().to_string());
                            last_error = Some(anyhow!("Failed to build buy instruction: {}", e));
                            
                            // If it's a pool reserves error, try with reduced amount
                            if e.to_string().contains("Pool has zero reserves") || e.to_string().contains("exceeds pool reserves") {
                                attempt_amount *= 0.5;
                                if attempt_amount < 0.001 {
                                    break;
                                }
                                continue;
                            }
                            
                            return Err(anyhow!("Failed to build buy instruction: {}", e));
                        }
                    }
                }
                
                // If we get here, all attempts failed
                Err(last_error.unwrap_or_else(|| anyhow!("All buy attempts failed")))
            },
            SwapProtocol::RaydiumLaunchpad => {
                let raydium = crate::dex::raydium_launchpad::Raydium::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                match raydium.build_swap_from_parsed_data(trade_info, buy_config).await {
                    Ok((keypair, instructions, price)) => {
                        self.logger.log(format!("Generated RaydiumLaunchpad buy instruction at price: {}", price));
                        
                        // Get recent blockhash
                        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        
                        // Execute with transaction landing mode
                        match crate::core::tx::new_signed_and_send_with_landing_mode(
                            self.transaction_landing_mode.clone(),
                            &self.app_state,
                            recent_blockhash,
                            &keypair,
                            instructions,
                            &self.logger,
                        ).await {
                            Ok(signatures) => {
                                if signatures.is_empty() {
                                    return Err(anyhow!("No transaction signature returned"));
                                }
                                
                                let signature = &signatures[0];
                                self.logger.log(format!("Buy transaction sent: {}", signature).green().to_string());
                                
                                // Track the bought token
                                self.track_bought_token(trade_info, &signature.to_string(), "RaydiumLaunchpad").await?;
                                
                                Ok(())
                            },
                            Err(e) => {
                                self.logger.log(format!("Buy transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send buy transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build RaydiumLaunchpad buy instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build buy instruction: {}", e))
                    }
                }
            },
            _ => Err(anyhow!("Unsupported protocol")),
        };
        
        result
    }
    
    /// Execute a sell operation when target sells with retry mechanism and Jupiter fallback
    pub async fn execute_sell(&self, trade_info: &TradeInfoFromToken) -> Result<()> {
        self.logger.log(format!("Executing SELL for token: {}", trade_info.mint).red().to_string());
        
        // First check if we actually own this token
        if !BOUGHT_TOKENS.has_token(&trade_info.mint) {
            self.logger.log(format!("We don't own token {}, skipping sell", trade_info.mint).yellow().to_string());
            return Ok(());
        }
        
        // Get token info
        let token_info = match BOUGHT_TOKENS.get_token_info(&trade_info.mint) {
            Some(info) => info,
            None => {
                self.logger.log(format!("Token info not found for {}, skipping sell", trade_info.mint).yellow().to_string());
                return Ok(());
            }
        };

        // Check if we have cached balance (required for selling without RPC calls)
        let cached_balance = BOUGHT_TOKENS.get_cached_balance(&trade_info.mint, 30);
        if cached_balance.is_none() {
            self.logger.log(format!("No cached balance found for {}, skipping sell", trade_info.mint).yellow().to_string());
            return Err(anyhow!("Cached balance required for selling but not found"));
        }

        let (token_amount_raw, token_amount, token_decimals) = cached_balance.unwrap();
        
        if token_amount <= 0.0 {
            self.logger.log("No tokens to sell, removing from tracking".yellow().to_string());
            BOUGHT_TOKENS.remove_token(&trade_info.mint);
            return Ok(());
        }

        self.logger.log(format!("Using cached balance for selling {} tokens ({} raw units)", 
            token_amount, token_amount_raw).cyan().to_string());

        // Create sell config
        let mut sell_config = (*self.swap_config).clone();
        sell_config.swap_direction = SwapDirection::Sell;
        sell_config.amount_in = token_amount;

        // Try native protocols first with retry mechanism
        let result = self.execute_sell_with_retries(trade_info, sell_config.clone()).await;
        
        match result {
            Ok(signature) => {
                self.logger.log(format!("Native sell successful: {}", signature).green().to_string());
                // Remove token from tracking after successful sell
                BOUGHT_TOKENS.remove_token(&trade_info.mint);
                Ok(())
            },
            Err(e) => {
                self.logger.log(format!("All native sell attempts failed: {}", e).red().to_string());
                
                // Try Jupiter as fallback only if we have valid balance info
                if token_amount_raw > 0 {
                    self.logger.log("Trying Jupiter as fallback...".magenta().to_string());
                    match self.execute_jupiter_sell(&trade_info.mint, token_amount_raw, token_decimals).await {
                        Ok(signature) => {
                            self.logger.log(format!("Jupiter sell fallback successful: {}", signature).green().to_string());
                            // Remove token from tracking after successful sell
                            BOUGHT_TOKENS.remove_token(&trade_info.mint);
                            Ok(())
                        },
                        Err(jupiter_error) => {
                            self.logger.log(format!("Jupiter sell fallback also failed: {}", jupiter_error).red().to_string());
                            Err(anyhow!("Both native and Jupiter sell failed. Native: {}, Jupiter: {}", e, jupiter_error))
                        }
                    }
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Execute sell with retries using native protocols
    async fn execute_sell_with_retries(&self, trade_info: &TradeInfoFromToken, sell_config: SwapConfig) -> Result<String> {
        const MAX_RETRIES: u32 = 3;
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=MAX_RETRIES {
            self.logger.log(format!("Sell attempt {}/{} for token: {}", attempt, MAX_RETRIES, trade_info.mint).cyan().to_string());
            
            let result = match self.get_protocol_from_trade_info(trade_info) {
                SwapProtocol::PumpFun => {
                    self.logger.log(format!("Sell attempt {} using PumpFun protocol", attempt).blue().to_string());
                    
                    let pump = Pump::new(
                        self.app_state.rpc_nonblocking_client.clone(),
                        self.app_state.rpc_client.clone(),
                        self.app_state.wallet.clone(),
                    );
                    
                    self.execute_protocol_sell(&pump, trade_info, sell_config.clone()).await
                },
                SwapProtocol::PumpSwap => {
                    self.logger.log(format!("Sell attempt {} using PumpSwap protocol", attempt).blue().to_string());
                    
                    let pump_swap = PumpSwap::new(
                        self.app_state.wallet.clone(),
                        Some(self.app_state.rpc_client.clone()),
                        Some(self.app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    self.execute_protocol_sell(&pump_swap, trade_info, sell_config.clone()).await
                },
                SwapProtocol::RaydiumLaunchpad => {
                    self.logger.log(format!("Sell attempt {} using RaydiumLaunchpad protocol", attempt).blue().to_string());
                    
                    let raydium = crate::dex::raydium_launchpad::Raydium::new(
                        self.app_state.wallet.clone(),
                        Some(self.app_state.rpc_client.clone()),
                        Some(self.app_state.rpc_nonblocking_client.clone()),
                    );
                    
                    self.execute_protocol_sell(&raydium, trade_info, sell_config.clone()).await
                },
                _ => {
                    Err(anyhow!("Unsupported protocol for sell: {:?}", self.get_protocol_from_trade_info(trade_info)))
                }
            };

            match result {
                Ok(signature) => {
                    // Verify transaction success
                    let jupiter_client = JupiterClient::new(self.app_state.rpc_nonblocking_client.clone());
                    match jupiter_client.verify_transaction(&signature).await {
                        Ok(true) => {
                            self.logger.log(format!("Sell attempt {} verified successfully: {}", attempt, signature).green().to_string());
                            return Ok(signature);
                        },
                        Ok(false) => {
                            let error_msg = format!("Sell attempt {} transaction failed verification: {}", attempt, signature);
                            self.logger.log(error_msg.clone().red().to_string());
                            // Invalidate cache since transaction failed
                            BOUGHT_TOKENS.invalidate_balance_cache(&trade_info.mint);
                            last_error = Some(anyhow!(error_msg));
                        },
                        Err(e) => {
                            let error_msg = format!("Sell attempt {} verification error: {}", attempt, e);
                            self.logger.log(error_msg.clone().red().to_string());
                            // Invalidate cache on verification error
                            BOUGHT_TOKENS.invalidate_balance_cache(&trade_info.mint);
                            last_error = Some(anyhow!(error_msg));
                        }
                    }
                },
                Err(e) => {
                    self.logger.log(format!("Sell attempt {} failed: {}", attempt, e).red().to_string());
                    // Invalidate cache since transaction failed
                    BOUGHT_TOKENS.invalidate_balance_cache(&trade_info.mint);
                    last_error = Some(e);
                }
            }

            // Wait before retry (except on last attempt)
            if attempt < MAX_RETRIES {
                self.logger.log(format!("Waiting 2 seconds before retry attempt {}...", attempt + 1).yellow().to_string());
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("All sell attempts failed")))
    }

    /// Execute protocol-specific sell transaction
    async fn execute_protocol_sell<T>(&self, protocol: &T, trade_info: &TradeInfoFromToken, sell_config: SwapConfig) -> Result<String>
    where
        T: ProtocolSell,
    {
        // Get cached balance first
        let cached_balance = if sell_config.swap_direction == SwapDirection::Sell {
            // Get cached balance for selling
            if let Some((raw, _decimal, decimals)) = BOUGHT_TOKENS.get_cached_balance(&trade_info.mint, 30) {
                Some((raw, decimals))
            } else {
                self.logger.log(format!("No cached balance found for {}, cannot build sell transaction without RPC", trade_info.mint).red().to_string());
                return Err(anyhow!("Cached balance required for selling, but not found"));
            }
        } else {
            None
        };

        // Build swap instruction using cached balance
        let (keypair, instructions, price) = protocol.build_swap_from_parsed_data_with_balance(trade_info, sell_config, cached_balance).await?;
        
        // Get recent blockhash
        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
            Some(hash) => hash,
            None => {
                return Err(anyhow!("Failed to get recent blockhash"));
            }
        };
        
        self.logger.log(format!("Generated sell instruction at price: {}", price));
        
        // Execute with transaction landing mode
        match crate::core::tx::new_signed_and_send_with_landing_mode(
            self.transaction_landing_mode.clone(),
            &self.app_state,
            recent_blockhash,
            &keypair,
            instructions,
            &self.logger,
        ).await {
            Ok(signatures) => {
                if signatures.is_empty() {
                    return Err(anyhow!("No transaction signature returned"));
                }
                
                Ok(signatures[0].clone())
            },
            Err(e) => {
                Err(anyhow!("Failed to send sell transaction: {}", e))
            }
        }
    }

    /// Execute Jupiter sell as fallback
    async fn execute_jupiter_sell(&self, token_mint: &str, token_amount_raw: u64, _token_decimals: u8) -> Result<String> {
        self.logger.log(format!("Executing Jupiter fallback sell for token: {} (amount: {})", token_mint, token_amount_raw).magenta().to_string());
        
        let jupiter_client = JupiterClient::new(self.app_state.rpc_nonblocking_client.clone());
        
        // Use 100 bps (1%) slippage for Jupiter
        let slippage_bps = 100;
        
        jupiter_client.sell_token_with_jupiter(
            token_mint,
            token_amount_raw,
            slippage_bps,
            &self.app_state.wallet
        ).await
    }
}

/// Trait for protocol-specific sell operations
trait ProtocolSell {
    fn build_swap_from_parsed_data(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig) -> impl std::future::Future<Output = Result<(Arc<Keypair>, Vec<Instruction>, f64)>> + Send;
    fn build_swap_from_parsed_data_with_balance(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig, cached_balance: Option<(u64, u8)>) -> impl std::future::Future<Output = Result<(Arc<Keypair>, Vec<Instruction>, f64)>> + Send;
}

impl ProtocolSell for Pump {
    async fn build_swap_from_parsed_data(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        self.build_swap_from_parsed_data(trade_info, swap_config).await
    }

    async fn build_swap_from_parsed_data_with_balance(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig, cached_balance: Option<(u64, u8)>) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        self.build_swap_from_parsed_data_with_balance(trade_info, swap_config, cached_balance).await
    }
}

impl ProtocolSell for PumpSwap {
    async fn build_swap_from_parsed_data(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        self.build_swap_from_parsed_data(trade_info, swap_config).await
    }

    async fn build_swap_from_parsed_data_with_balance(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig, cached_balance: Option<(u64, u8)>) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        self.build_swap_from_parsed_data_with_balance(trade_info, swap_config, cached_balance).await
    }
}

impl ProtocolSell for crate::dex::raydium_launchpad::Raydium {
    async fn build_swap_from_parsed_data(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        self.build_swap_from_parsed_data(trade_info, swap_config).await
    }

    async fn build_swap_from_parsed_data_with_balance(&self, trade_info: &TradeInfoFromToken, swap_config: SwapConfig, cached_balance: Option<(u64, u8)>) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        // Raydium should also implement the new method, for now fallback to old method
        self.build_swap_from_parsed_data(trade_info, swap_config).await
    }
}

impl SimpleSellingEngine {
    /// Track a bought token and cache its balance after verification
    async fn track_bought_token(&self, trade_info: &TradeInfoFromToken, signature: &str, protocol: &str) -> Result<()> {
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;

        // Get token account address
        let token_pubkey = Pubkey::from_str(&trade_info.mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);
        
        // Add to bought tokens tracking
        BOUGHT_TOKENS.add_bought_token(
            trade_info.mint.clone(),
            ata,
            trade_info.token_change.abs(),
            signature.to_string(),
            protocol.to_string(),
        );
        
        self.logger.log(format!("Added token {} to bought tokens tracking", trade_info.mint).green().to_string());
        
        // Verify transaction and cache balance for fast selling
        self.verify_and_cache_balance(&trade_info.mint, &ata).await?;
        
        Ok(())
    }

    /// Verify buy transaction and cache the resulting balance for fast selling
    async fn verify_and_cache_balance(&self, mint: &str, ata: &Pubkey) -> Result<()> {
        self.logger.log(format!("Verifying and caching balance for token: {}", mint).blue().to_string());
        
        // Wait a bit for transaction to settle
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        
        // Get token account to cache balance
        match self.app_state.rpc_nonblocking_client.get_token_account(ata).await {
            Ok(Some(account)) => {
                let amount_raw = account.token_amount.amount.parse::<u64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?;
                let decimal_amount = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?
                    / 10f64.powi(account.token_amount.decimals as i32);
                let decimals = account.token_amount.decimals;
                
                // Cache the verified balance
                BOUGHT_TOKENS.cache_verified_balance(mint, amount_raw, decimal_amount, decimals);
                self.logger.log(format!("Cached verified balance for {}: {} tokens ({} raw units)", 
                    mint, decimal_amount, amount_raw).green().to_string());
                
                Ok(())
            },
            Ok(None) => {
                self.logger.log(format!("Token account not found for {}, balance cache skipped", mint).yellow().to_string());
                Ok(())
            },
            Err(e) => {
                self.logger.log(format!("Failed to verify balance for {}: {}", mint, e).yellow().to_string());
                Ok(()) // Don't fail the whole buy process if balance verification fails
            }
        }
    }

    /// Get token balance with low latency - tries cache first, falls back to RPC
    async fn get_token_balance_fast(&self, mint: &str, ata: &Pubkey) -> Result<(u64, f64, u8)> {
        // Try cached balance first (max age: 30 seconds for selling speed)
        if let Some((raw, decimal, decimals)) = BOUGHT_TOKENS.get_cached_balance(mint, 30) {
            self.logger.log(format!("Using cached balance for {}: {} tokens (age < 30s)", mint, decimal).green().to_string());
            return Ok((raw, decimal, decimals));
        }

        // Cache miss or stale - fetch from RPC
        self.logger.log(format!("Cache miss for {}, fetching balance from RPC", mint).yellow().to_string());
        match self.app_state.rpc_nonblocking_client.get_token_account(ata).await {
            Ok(Some(account)) => {
                let amount_raw = account.token_amount.amount.parse::<u64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?;
                let decimal_amount = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?
                    / 10f64.powi(account.token_amount.decimals as i32);
                let decimals = account.token_amount.decimals;
                
                // Update cache with fresh data
                BOUGHT_TOKENS.cache_verified_balance(mint, amount_raw, decimal_amount, decimals);
                self.logger.log(format!("Fetched and cached fresh balance for {}: {} tokens ({} raw units)", 
                    mint, decimal_amount, amount_raw).blue().to_string());
                
                Ok((amount_raw, decimal_amount, decimals))
            },
            Ok(None) => {
                self.logger.log(format!("No token account found for mint: {}, removing from tracking", mint).yellow().to_string());
                BOUGHT_TOKENS.remove_token(mint);
                Err(anyhow!("Token account not found"))
            },
            Err(e) => {
                Err(anyhow!("Failed to get token account: {}", e))
            }
        }
    }
    
    /// Determine protocol from trade info
    fn get_protocol_from_trade_info(&self, trade_info: &TradeInfoFromToken) -> SwapProtocol {
        match trade_info.dex_type {
            DexType::PumpSwap => SwapProtocol::PumpSwap,
            DexType::PumpFun => SwapProtocol::PumpFun,
            DexType::RaydiumLaunchpad => SwapProtocol::RaydiumLaunchpad,
            _ => self.app_state.protocol_preference.clone(),
        }
    }
} 
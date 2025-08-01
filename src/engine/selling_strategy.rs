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
        
        // Limit the buy amount to prevent BuyMoreBaseAmountThanPoolReserves errors
        let limited_amount = trade_info.token_amount_f64.min(buy_config.max_buy_amount);
        if limited_amount != trade_info.token_amount_f64 {
            self.logger.log(format!("Limited buy amount from {} to {} SOL (max_buy_amount)", 
                trade_info.token_amount_f64, limited_amount).yellow().to_string());
        }
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
            _ => Err(anyhow!("Unsupported protocol")),
        };
        
        result
    }
    
    /// Execute a sell operation when target sells
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
        
        // Get wallet pubkey
        let wallet_pubkey = self.app_state.wallet.try_pubkey()
            .map_err(|e| anyhow!("Failed to get wallet pubkey: {}", e))?;

        // Get token account to determine actual balance
        let token_pubkey = Pubkey::from_str(&trade_info.mint)
            .map_err(|e| anyhow!("Invalid token mint address: {}", e))?;
        let ata = get_associated_token_address(&wallet_pubkey, &token_pubkey);

        // Get current token balance
        let token_amount = match self.app_state.rpc_nonblocking_client.get_token_account(&ata).await {
            Ok(Some(account)) => {
                let amount_value = account.token_amount.amount.parse::<f64>()
                    .map_err(|e| anyhow!("Failed to parse token amount: {}", e))?;
                let decimal_amount = amount_value / 10f64.powi(account.token_amount.decimals as i32);
                self.logger.log(format!("Selling {} tokens", decimal_amount).red().to_string());
                decimal_amount
            },
            Ok(None) => {
                self.logger.log(format!("No token account found for mint: {}, removing from tracking", trade_info.mint).yellow().to_string());
                BOUGHT_TOKENS.remove_token(&trade_info.mint);
                return Ok(());
            },
            Err(e) => {
                return Err(anyhow!("Failed to get token account: {}", e));
            }
        };

        if token_amount <= 0.0 {
            self.logger.log("No tokens to sell, removing from tracking".yellow().to_string());
            BOUGHT_TOKENS.remove_token(&trade_info.mint);
            return Ok(());
        }

        // Create sell config
        let mut sell_config = (*self.swap_config).clone();
        sell_config.swap_direction = SwapDirection::Sell;
        sell_config.amount_in = token_amount;

        // Execute sell based on protocol
        let result = match self.get_protocol_from_trade_info(trade_info) {
            SwapProtocol::PumpFun => {
                self.logger.log("Using PumpFun protocol for sell".red().to_string());
                
                let pump = Pump::new(
                    self.app_state.rpc_nonblocking_client.clone(),
                    self.app_state.rpc_client.clone(),
                    self.app_state.wallet.clone(),
                );
                
                match pump.build_swap_from_parsed_data(trade_info, sell_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash
                        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        self.logger.log(format!("Generated PumpFun sell instruction at price: {}", price));
                        
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
                                self.logger.log(format!("Sell transaction sent: {}", signature).green().to_string());
                                
                                // Remove token from tracking after successful sell
                                BOUGHT_TOKENS.remove_token(&trade_info.mint);
                                self.logger.log(format!("Removed token {} from tracking after successful sell", trade_info.mint));
                                
                                Ok(())
                            },
                            Err(e) => {
                                self.logger.log(format!("Sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build PumpFun sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build sell instruction: {}", e))
                    }
                }
            },
            SwapProtocol::PumpSwap => {
                self.logger.log("Using PumpSwap protocol for sell".red().to_string());
                
                let pump_swap = PumpSwap::new(
                    self.app_state.wallet.clone(),
                    Some(self.app_state.rpc_client.clone()),
                    Some(self.app_state.rpc_nonblocking_client.clone()),
                );
                
                match pump_swap.build_swap_from_parsed_data(trade_info, sell_config).await {
                    Ok((keypair, instructions, price)) => {
                        // Get recent blockhash
                        let recent_blockhash = match crate::services::blockhash_processor::BlockhashProcessor::get_latest_blockhash().await {
                            Some(hash) => hash,
                            None => {
                                self.logger.log("Failed to get recent blockhash".red().to_string());
                                return Err(anyhow!("Failed to get recent blockhash"));
                            }
                        };
                        self.logger.log(format!("Generated PumpSwap sell instruction at price: {}", price));
                        
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
                                self.logger.log(format!("Sell transaction sent: {}", signature).green().to_string());
                                
                                // Remove token from tracking after successful sell
                                BOUGHT_TOKENS.remove_token(&trade_info.mint);
                                self.logger.log(format!("Removed token {} from tracking after successful sell", trade_info.mint));
                                
                                Ok(())
                            },
                            Err(e) => {
                                self.logger.log(format!("Sell transaction failed: {}", e).red().to_string());
                                Err(anyhow!("Failed to send sell transaction: {}", e))
                            }
                        }
                    },
                    Err(e) => {
                        self.logger.log(format!("Failed to build PumpSwap sell instruction: {}", e).red().to_string());
                        Err(anyhow!("Failed to build sell instruction: {}", e))
                    }
                }
            },
            _ => {
                return Err(anyhow!("Unsupported protocol for sell: {:?}", self.get_protocol_from_trade_info(trade_info)));
            }
        };

        // Update token account list after sell
        if result.is_ok() {
            WALLET_TOKEN_ACCOUNTS.remove(&ata);
            self.logger.log(format!("Removed token account {} from global list after sell", ata));
        }

        result
    }
    
    /// Track a bought token
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
            trade_info.token_amount_f64,
            signature.to_string(),
            protocol.to_string(),
        );
        
        self.logger.log(format!("Added token {} to bought tokens tracking", trade_info.mint).green().to_string());
        
        Ok(())
    }
    
    /// Determine protocol from trade info
    fn get_protocol_from_trade_info(&self, trade_info: &TradeInfoFromToken) -> SwapProtocol {
        match trade_info.dex_type {
            DexType::PumpSwap => SwapProtocol::PumpSwap,
            DexType::PumpFun => SwapProtocol::PumpFun,
            _ => self.app_state.protocol_preference.clone(),
        }
    }
} 
use std::{str::FromStr, sync::Arc};
use solana_program_pack::Pack;
use anchor_client::solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use anchor_client::solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_account_decoder::UiAccountEncoding;
use anyhow::{anyhow, Result};
use colored::Colorize;
use anchor_client::solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    system_program,
    signer::Signer,
};
use crate::engine::transaction_parser::DexType;
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent
};
use spl_token::ui_amount_to_amount;
use tokio::sync::OnceCell;
use lru::LruCache;
use std::num::NonZeroUsize;

use crate::{
    common::{config::SwapConfig, logger::Logger, cache::WALLET_TOKEN_ACCOUNTS},
    core::token,
    engine::swap::{SwapDirection, SwapInType},
};

// Constants - moved to lazy_static for single initialization
lazy_static::lazy_static! {
    static ref TOKEN_PROGRAM: Pubkey = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
    static ref TOKEN_2022_PROGRAM: Pubkey = Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();
    static ref ASSOCIATED_TOKEN_PROGRAM: Pubkey = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
    static ref PUMP_SWAP_PROGRAM: Pubkey = Pubkey::from_str("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA").unwrap();
    static ref PUMP_GLOBAL_CONFIG: Pubkey = Pubkey::from_str("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw").unwrap();
    static ref PUMP_FEE_RECIPIENT: Pubkey = Pubkey::from_str("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV").unwrap();
    static ref PUMP_EVENT_AUTHORITY: Pubkey = Pubkey::from_str("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR").unwrap();
    static ref SOL_MINT: Pubkey = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    static ref BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
    static ref SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
}

// Thread-safe cache with LRU eviction policy
static TOKEN_ACCOUNT_CACHE: OnceCell<LruCache<Pubkey, bool>> = OnceCell::const_new();

const TEN_THOUSAND: u64 = 10000;
const CACHE_SIZE: usize = 1000;

async fn init_caches() {
    TOKEN_ACCOUNT_CACHE.get_or_init(|| async {
        LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())
    }).await;
}

/// A struct to represent the PumpSwap pool which uses constant product AMM
#[derive(Debug, Clone)]
pub struct PumpSwapPool {
    pub pool_id: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_account: Pubkey,
    pub pool_quote_account: Pubkey,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub coin_creator: Pubkey,
}

pub struct PumpSwap {
    pub keypair: Arc<Keypair>,
    pub rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
    pub rpc_nonblocking_client: Option<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>>,
}

impl PumpSwap {
    pub fn new(
        keypair: Arc<Keypair>,
        rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
        rpc_nonblocking_client: Option<Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>>,
    ) -> Self {
        // Initialize caches on first use
        tokio::spawn(init_caches());
        
        Self {
            keypair,
            rpc_client,
            rpc_nonblocking_client,
        }
    }

    pub async fn get_pump_swap_pool(
        &self,
        mint_str: &str,
    ) -> Result<PumpSwapPool> {
        let mint = Pubkey::from_str(mint_str).map_err(|_| anyhow!("Invalid mint address"))?;
        let rpc_client = self.rpc_client.clone()
            .ok_or_else(|| anyhow!("RPC client not initialized"))?;
        get_pool_info(rpc_client, mint).await
    }

    pub async fn get_token_price(&self, mint_str: &str) -> Result<f64> {
        let pool = self.get_pump_swap_pool(mint_str).await?;
        
        // Calculate price using pool reserves
        if pool.base_reserve == 0 {
            return Ok(0.0);
        }
        
        // Price formula: quote_reserve / base_reserve
        let price = pool.quote_reserve as f64 / pool.base_reserve as f64;
        Ok(price)
    }

    async fn get_or_fetch_pool_info(
        &self,
        trade_info: &crate::engine::transaction_parser::TradeInfoFromToken,
        mint: Pubkey
    ) -> Result<PumpSwapPool> {
        if let Some(info) = &trade_info.pool_info {
            Ok(PumpSwapPool {
                pool_id: info.pool_id,
                base_mint: mint,
                quote_mint: *SOL_MINT,
                lp_mint: Pubkey::default(),
                pool_base_account: get_associated_token_address(&info.pool_id, &mint),
                pool_quote_account: get_associated_token_address(&info.pool_id, &SOL_MINT),
                base_reserve: info.base_reserve,
                quote_reserve: info.quote_reserve,
                coin_creator: info.coin_creator,
            })
        } else {
            let rpc_client = self.rpc_client.clone()
                .ok_or_else(|| anyhow!("RPC client not initialized"))?;
            get_pool_info(rpc_client, mint).await
        }
    }

    // Highly optimized build_swap_from_parsed_data
    pub async fn build_swap_from_parsed_data(
        &self,
        trade_info: &crate::engine::transaction_parser::TradeInfoFromToken,
        swap_config: SwapConfig,
    ) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        let logger = Logger::new("[PUMPSWAP-FROM-PARSED] => ".blue().to_string());
        let start_time = std::time::Instant::now();
        
        // Early validation
        if trade_info.dex_type != DexType::PumpSwap {
            return Err(anyhow!("Invalid transaction type"));
        }
        
        let mint = Pubkey::from_str(&trade_info.mint)?;
        let owner = self.keypair.pubkey();
        
        // Parallelize expensive operations
        let pool_info = self.get_or_fetch_pool_info(trade_info, mint).await?;
        
        // Validate pool info
        if pool_info.base_reserve == 0 || pool_info.quote_reserve == 0 {
            return Err(anyhow!("Pool has zero reserves for mint {}", trade_info.mint));
        }
        
        // Calculate price in parallel with account setup
        let token_price = tokio::task::spawn_blocking(move || {
            if pool_info.base_reserve > 0 {
                pool_info.quote_reserve as f64 / pool_info.base_reserve as f64
            } else {
                0.0
            }
        }).await?;
        
        logger.log(format!("Pool reserves - Base: {}, Quote: {}", 
        pool_info.base_reserve, pool_info.quote_reserve));
        logger.log(format!("Token price: {} SOL per token", token_price));
        
        // Prepare swap parameters
        let (_token_in, _token_out, discriminator) = match swap_config.swap_direction {
            SwapDirection::Buy => (*SOL_MINT, mint, *BUY_DISCRIMINATOR),
            SwapDirection::Sell => (mint, *SOL_MINT, *SELL_DISCRIMINATOR),
        };
        
        let mut instructions = Vec::with_capacity(3); // Pre-allocate for typical case
        
        // Process swap direction
        let (base_amount, quote_amount, accounts) = match swap_config.swap_direction {
            SwapDirection::Buy => {
                // For buys, validate that we're not trying to buy more than available
                if swap_config.amount_in <= 0.0 {
                    return Err(anyhow!("Invalid buy amount: {}", swap_config.amount_in));
                }
                
                self.prepare_buy_swap(
                    &pool_info,
                    owner,
                    mint,
                    swap_config.amount_in,
                    swap_config.slippage as u64,
                    &mut instructions,
                ).await?
            },
            SwapDirection::Sell => self.prepare_sell_swap(
                &pool_info,
                owner,
                mint,
                swap_config.amount_in,
                swap_config.in_type,
                swap_config.slippage as u64,
                &mut instructions,
            ).await?,
        };
        
        // Add swap instruction if amount is valid
        if base_amount > 0 {
            logger.log(format!("Creating swap instruction - Base amount: {}, Quote amount: {}", 
                base_amount, quote_amount).green().to_string());
            
            instructions.push(create_swap_instruction(
                *PUMP_SWAP_PROGRAM,
            discriminator,
            base_amount,
            quote_amount,
            accounts,
            ));
        } else {
            return Err(anyhow!("Invalid swap amount: {}", base_amount));
        }
        
        logger.log(format!("Build time: {:?}", start_time.elapsed()).blue().to_string());
        Ok((self.keypair.clone(), instructions, token_price))
    }
    
    // Helper methods broken down for better optimization
    
    async fn prepare_buy_swap(
        &self,
        pool_info: &PumpSwapPool,
        owner: Pubkey,
        mint: Pubkey,
        amount_in: f64,
        slippage_bps: u64,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(u64, u64, Vec<AccountMeta>)> {
        let logger = Logger::new("[PUMPSWAP-PREPARE-BUY] => ".blue().to_string());
        
        let amount_specified = ui_amount_to_amount(amount_in, 9);
        
        // Log pool information for debugging
        logger.log(format!("Pool reserves - Base: {}, Quote: {}", 
            pool_info.base_reserve, pool_info.quote_reserve));
        logger.log(format!("Input amount: {} SOL ({} lamports)", amount_in, amount_specified));
        
        // Validate pool reserves
        if pool_info.base_reserve == 0 || pool_info.quote_reserve == 0 {
            return Err(anyhow!("Pool has zero reserves"));
        }
        
        // Calculate base amount out
        let base_amount_out = calculate_buy_base_amount(
            amount_specified, 
            pool_info.quote_reserve, 
            pool_info.base_reserve
        );
        
        logger.log(format!("Calculated base amount out: {}", base_amount_out));
        
        // Additional safety checks
        if base_amount_out == 0 {
            return Err(anyhow!("Calculated output amount is zero"));
        }
        
        if base_amount_out > pool_info.base_reserve {
            logger.log(format!("Warning: Calculated amount {} exceeds pool reserves {}, limiting to reserves", 
                base_amount_out, pool_info.base_reserve).yellow().to_string());
            // Don't return error, just limit the amount
        }
        
        // Ensure we don't exceed 95% of pool reserves to be safe
        let max_safe_amount = (pool_info.base_reserve as f64 * 0.95) as u64;
        let final_base_amount = if base_amount_out > max_safe_amount {
            logger.log(format!("Limiting buy amount from {} to {} (95% of reserves)", 
                base_amount_out, max_safe_amount).yellow().to_string());
            max_safe_amount
        } else {
            base_amount_out
        };
        
        let max_quote_amount_in = max_amount_with_slippage(amount_specified, slippage_bps);
        let out_ata = get_associated_token_address(&owner, &mint);
        
        // Check token account existence without RPC call if possible
        if !self.check_token_account_cache(out_ata).await {
            instructions.push(create_associated_token_account_idempotent(
                &owner,
                &owner,
                &mint,
                &TOKEN_PROGRAM,
            ));
            self.cache_token_account(out_ata).await;
        }
        
        let accounts = create_buy_accounts(
            pool_info.pool_id,
            owner,
            mint,
            *SOL_MINT,
            out_ata,
            get_associated_token_address(&owner, &SOL_MINT),
            pool_info.pool_base_account,
            pool_info.pool_quote_account,
            pool_info.coin_creator,
        )?;
        
        logger.log(format!("Final buy parameters - Base amount: {}, Max quote: {}", 
            final_base_amount, max_quote_amount_in).green().to_string());
        
        Ok((final_base_amount, max_quote_amount_in, accounts))
    }
    
    async fn prepare_sell_swap(
        &self,
        pool_info: &PumpSwapPool,
        owner: Pubkey,
        mint: Pubkey,
        amount_in: f64,
        in_type: SwapInType,
        slippage_bps: u64,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(u64, u64, Vec<AccountMeta>)> {
        let in_ata = get_associated_token_address(&owner, &mint);
        
        // Verify token account exists using cache first
        if !self.check_token_account_cache(in_ata).await {
            return Err(anyhow!("Token account does not exist"));
        }
        
        // Get token info in parallel
        let (account_info, mint_info) = if let Some(client) = &self.rpc_nonblocking_client {
            let account_fut = token::get_account_info(client.clone(), mint, in_ata);
            let mint_fut = token::get_mint_info(client.clone(), self.keypair.clone(), mint);
            tokio::try_join!(account_fut, mint_fut)?
        } else {
            return Err(anyhow!("RPC client not available"));
        };
        
        let amount = match in_type {
            SwapInType::Qty => ui_amount_to_amount(amount_in, mint_info.base.decimals),
            SwapInType::Pct => {
                let pct = amount_in.min(1.0);
                if pct == 1.0 {
                    // Close account if selling 100%
                    instructions.push(spl_token::instruction::close_account(
                        &TOKEN_PROGRAM,
                        &in_ata,
                        &owner,
                        &owner,
                        &[&owner],
                    )?);
                    account_info.base.amount
                } else {
                    (pct * account_info.base.amount as f64) as u64
                }
            }
        };
        
        if amount == 0 {
            return Err(anyhow!("Invalid sell amount"));
        }
        
        let quote_amount_out = calculate_sell_quote_amount(
            amount, 
            pool_info.base_reserve, 
            pool_info.quote_reserve
        );
        
        let min_quote_amount_out = min_amount_with_slippage(quote_amount_out, slippage_bps);
        println!("min_quote_amount_out: {:?}", min_quote_amount_out);
        println!("quote_amount_out: {:?}", quote_amount_out);
        println!("amount: {:?}", amount);
        println!("base_reserve: {:?}", pool_info.pool_base_account);
        println!("quote_reserve: {:?}", pool_info.pool_quote_account);       
        println!("coin_creator: {:?}", pool_info.coin_creator);

        let accounts = create_sell_accounts(
            pool_info.pool_id,
            owner,
            mint,
            *SOL_MINT,
            in_ata,
            get_associated_token_address(&owner, &SOL_MINT),
            pool_info.pool_base_account,
            pool_info.pool_quote_account,
            pool_info.coin_creator,
        )?;
        
        Ok((amount, min_quote_amount_out, accounts))
    }
    
    async fn check_token_account_cache(&self, account: Pubkey) -> bool {
        WALLET_TOKEN_ACCOUNTS.contains(&account)
    }
    
    async fn cache_token_account(&self, account: Pubkey) {
        WALLET_TOKEN_ACCOUNTS.insert(account);
    }
}

/// Get the PumpSwap pool information for a specific token mint
pub async fn get_pool_info(
    rpc_client: Arc<anchor_client::solana_client::rpc_client::RpcClient>,
    mint: Pubkey,
) -> Result<PumpSwapPool> {
    let logger = Logger::new("[PUMPSWAP-GET-POOL-INFO] => ".blue().to_string());
    
    // Initialize
    let sol_mint = *SOL_MINT;
    let pump_program = *PUMP_SWAP_PROGRAM;
    
    // Use getProgramAccounts with config for better efficiency
    let mut pool_id = Pubkey::default();
    let mut retry_count = 0;
    let max_retries = 2;
    
    // Try to find the pool
    while retry_count < max_retries && pool_id == Pubkey::default() {
        match rpc_client.get_program_accounts_with_config(
            &pump_program,
            RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::DataSize(300),
                    RpcFilterType::Memcmp(Memcmp::new(43, MemcmpEncodedBytes::Base64(base64::encode(mint.to_bytes())))),
                ]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    ..Default::default()
                },
                ..Default::default()
            },
        ) {
            Ok(accounts) => {
                for (pubkey, account) in accounts.iter() {
                    if account.data.len() >= 75 {
                        if let Ok(pubkey_from_data) = Pubkey::try_from(&account.data[43..75]) {
                            if pubkey_from_data == mint {
                                pool_id = *pubkey;
                                break;
                            }
                        }
                    }
                }
                
                if pool_id != Pubkey::default() {
                    break;
                } else if retry_count + 1 < max_retries {
                    logger.log("No pools found for the given mint, retrying...".to_string());
                }
            }
            Err(err) => {
                logger.log(format!("Error getting program accounts (attempt {}/{}): {}", 
                                 retry_count + 1, max_retries, err));
            }
        }
        
        retry_count += 1;
        if retry_count < max_retries {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    
    if pool_id == Pubkey::default() {
        return Err(anyhow!("Failed to find PumpSwap pool for mint {}", mint));
    }
    
    // Derive token accounts
    let pool_base_account = get_associated_token_address(&pool_id, &mint);
    let pool_quote_account = get_associated_token_address(&pool_id, &sol_mint);
    
    // Get token balances with a single batch call
    let accounts = rpc_client.get_multiple_accounts(&[pool_base_account, pool_quote_account])?;
    
    // Extract balances with proper error handling
    let base_balance = if let Some(account_data) = &accounts[0] {
        match spl_token::state::Account::unpack(&account_data.data) {
            Ok(token_account) => token_account.amount,
            Err(e) => {
                logger.log(format!("Warning: Failed to unpack base token account: {}", e));
                10_000_000_000_000 // Fallback value
            }
        }
    } else {
        logger.log("Warning: Base token account not found".to_string());
        10_000_000_000_000 // Fallback value
    };
    
    let quote_balance = if let Some(account_data) = &accounts[1] {
        match spl_token::state::Account::unpack(&account_data.data) {
            Ok(token_account) => token_account.amount,
            Err(e) => {
                logger.log(format!("Warning: Failed to unpack quote token account: {}", e));
                10_000_000_000 // Fallback value (10 SOL)
            }
        }
    } else {
        logger.log("Warning: Quote token account not found".to_string());
        10_000_000_000 // Fallback value (10 SOL)
    };
    
    Ok(PumpSwapPool {
        pool_id,
        base_mint: mint,
        quote_mint: sol_mint,
        lp_mint: Pubkey::default(),
        pool_base_account,
        pool_quote_account,
        base_reserve: base_balance,
        quote_reserve: quote_balance,
        coin_creator: Pubkey::default(),
    })
}

// Optimized math functions with overflow protection
#[inline]
fn calculate_buy_base_amount(quote_amount_in: u64, quote_reserve: u64, base_reserve: u64) -> u64 {
    if quote_amount_in == 0 || base_reserve == 0 || quote_reserve == 0 {
        return 0;
    }
    
    // Use the constant product formula: (x + dx) * (y - dy) = x * y
    // Where x = quote_reserve, y = base_reserve, dx = quote_amount_in, dy = base_amount_out
    // Solving for dy: dy = y - (x * y) / (x + dx)
    
    let quote_reserve_after = quote_reserve.saturating_add(quote_amount_in);
    let numerator = (quote_reserve as u128).saturating_mul(base_reserve as u128);
    let denominator = quote_reserve_after as u128;
    
    if denominator == 0 {
        return 0;
    }
    
    let base_reserve_after = numerator.checked_div(denominator).unwrap_or(0);
    let base_amount_out = base_reserve.saturating_sub(base_reserve_after as u64);
    
    // Ensure we don't exceed pool reserves (safety check)
    if base_amount_out > base_reserve {
        return base_reserve;
    }
    
    base_amount_out
}

#[inline]
fn calculate_sell_quote_amount(base_amount_in: u64, base_reserve: u64, quote_reserve: u64) -> u64 {
    if base_amount_in == 0 || base_reserve == 0 || quote_reserve == 0 {
        return 0;
    }
    
    let base_reserve_after = base_reserve.saturating_add(base_amount_in);
    let numerator = (quote_reserve as u128).saturating_mul(base_reserve as u128);
    let denominator = base_reserve_after as u128;
    
    if denominator == 0 {
        return 0;
    }
    
    let quote_reserve_after = numerator.checked_div(denominator).unwrap_or(0);
    quote_reserve.saturating_sub(quote_reserve_after as u64)
}

#[inline]
fn min_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .saturating_mul(TEN_THOUSAND.saturating_sub(slippage_bps))
        .checked_div(TEN_THOUSAND)
        .unwrap_or(0)
}

#[inline]
fn max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .saturating_mul(TEN_THOUSAND.saturating_add(slippage_bps))
        .checked_div(TEN_THOUSAND)
        .unwrap_or(input_amount)
}

// Optimized account creation with const pubkeys
fn create_buy_accounts(
    pool_id: Pubkey,
    user: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    user_base_token_account: Pubkey,
    wsol_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    coin_creator: Pubkey,
) -> Result<Vec<AccountMeta>> {
    let (coin_creator_vault_authority, _) = Pubkey::find_program_address(
        &[b"creator_vault", coin_creator.as_ref()],
        &PUMP_SWAP_PROGRAM,
    );
    let coin_creator_vault_ata = get_associated_token_address(&coin_creator_vault_authority, &quote_mint);
    
    Ok(vec![
        AccountMeta::new_readonly(pool_id, false),
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(*PUMP_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(user_base_token_account, false),
        AccountMeta::new(wsol_account, false),
        AccountMeta::new(pool_base_token_account, false),
        AccountMeta::new(pool_quote_token_account, false),
        AccountMeta::new_readonly(*PUMP_FEE_RECIPIENT, false),
        AccountMeta::new(get_associated_token_address(&PUMP_FEE_RECIPIENT, &quote_mint), false),
        AccountMeta::new_readonly(*TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(*TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(*ASSOCIATED_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(*PUMP_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PUMP_SWAP_PROGRAM, false),
        AccountMeta::new(coin_creator_vault_ata, false),
        AccountMeta::new_readonly(coin_creator_vault_authority, false),
        ])
}

// Similar optimization for sell accounts
fn create_sell_accounts(
    pool_id: Pubkey,
    user: Pubkey,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    user_base_token_account: Pubkey,
    wsol_account: Pubkey,
    pool_base_token_account: Pubkey,
    pool_quote_token_account: Pubkey,
    coin_creator: Pubkey,
) -> Result<Vec<AccountMeta>> {

    let (coin_creator_vault_authority, _) = Pubkey::find_program_address(
        &[b"creator_vault", coin_creator.as_ref()],
        &PUMP_SWAP_PROGRAM,
    );
    let coin_creator_vault_ata = get_associated_token_address(&coin_creator_vault_authority, &quote_mint);

    Ok(vec![
        AccountMeta::new_readonly(pool_id, false),
        AccountMeta::new(user, true),
        AccountMeta::new_readonly(*PUMP_GLOBAL_CONFIG, false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(user_base_token_account, false),
         AccountMeta::new(wsol_account, false),
         AccountMeta::new(pool_base_token_account, false),
         AccountMeta::new(pool_quote_token_account, false),
        AccountMeta::new_readonly(*PUMP_FEE_RECIPIENT, false),
        AccountMeta::new(get_associated_token_address(&PUMP_FEE_RECIPIENT, &quote_mint), false),
        AccountMeta::new_readonly(*TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(*TOKEN_PROGRAM, false),
         AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(*ASSOCIATED_TOKEN_PROGRAM, false),
        AccountMeta::new_readonly(*PUMP_EVENT_AUTHORITY, false),
        AccountMeta::new_readonly(*PUMP_SWAP_PROGRAM, false),
         AccountMeta::new(coin_creator_vault_ata, false),
         AccountMeta::new_readonly(coin_creator_vault_authority, false),
])
}

// Optimized instruction creation
fn create_swap_instruction(
    program_id: Pubkey,
    discriminator: [u8; 8],
    base_amount: u64,
    quote_amount: u64,
    accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&discriminator);
    data.extend_from_slice(&base_amount.to_le_bytes());
    data.extend_from_slice(&quote_amount.to_le_bytes());
    
    Instruction { program_id, accounts, data }
}


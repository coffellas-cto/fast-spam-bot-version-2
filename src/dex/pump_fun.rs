use std::{str::FromStr, sync::Arc};
use anyhow::{anyhow, Result};
use borsh::from_slice;
use tokio::time::Instant;
use borsh_derive::{BorshDeserialize, BorshSerialize};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use anchor_client::solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program,
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::{ui_amount_to_amount};
use tokio::sync::OnceCell;
use lru::LruCache;
use std::num::NonZeroUsize;

use crate::{
    common::{config::SwapConfig, logger::Logger, cache::WALLET_TOKEN_ACCOUNTS},
    core::token,
    engine::{monitor::BondingCurveInfo, swap::{SwapDirection, SwapInType}},
};

// Constants for cache
const CACHE_SIZE: usize = 1000;

// Thread-safe cache with LRU eviction policy
static TOKEN_ACCOUNT_CACHE: OnceCell<LruCache<Pubkey, bool>> = OnceCell::const_new();

async fn init_caches() {
    TOKEN_ACCOUNT_CACHE.get_or_init(|| async {
        LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())
    }).await;
}

pub const TEN_THOUSAND: u64 = 10000;
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const RENT_PROGRAM: &str = "SysvarRent111111111111111111111111111111111";
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const PUMP_GLOBAL: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
pub const PUMP_FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
pub const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
// pub const PUMP_FUN_MINT_AUTHORITY: &str = "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM";
pub const PUMP_EVENT_AUTHORITY: &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
pub const PUMP_BUY_METHOD: u64 = 16927863322537952870;
pub const PUMP_SELL_METHOD: u64 = 12502976635542562355;
pub const PUMP_FUN_CREATE_IX_DISCRIMINATOR: &[u8] = &[24, 30, 200, 40, 5, 28, 7, 119];
pub const INITIAL_VIRTUAL_SOL_RESERVES: u64 = 30_000_000_000;
pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000;
pub const TOKEN_TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;


#[derive(Clone)]
pub struct Pump {
    pub rpc_nonblocking_client: Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    pub keypair: Arc<Keypair>,
    pub rpc_client: Option<Arc<anchor_client::solana_client::rpc_client::RpcClient>>,
}

impl Pump {
    pub fn new(
        rpc_nonblocking_client: Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
        rpc_client: Arc<anchor_client::solana_client::rpc_client::RpcClient>,
        keypair: Arc<Keypair>,
    ) -> Self {
        // Initialize caches on first use
        tokio::spawn(init_caches());
        
        Self {
            rpc_nonblocking_client,
            keypair,
            rpc_client: Some(rpc_client),
        }
    }

    async fn check_token_account_cache(&self, account: Pubkey) -> bool {
        WALLET_TOKEN_ACCOUNTS.contains(&account)
    }

    async fn cache_token_account(&self, account: Pubkey) {
        WALLET_TOKEN_ACCOUNTS.insert(account);
    }

    pub async fn get_token_price(&self, mint_str: &str) -> Result<f64> {
        let mint = Pubkey::from_str(mint_str).map_err(|_| anyhow!("Invalid mint address"))?;
        let pump_program = Pubkey::from_str(PUMP_PROGRAM)?;
        
        // Get the bonding curve account info
        let (_, _, bonding_curve_reserves) = get_bonding_curve_account(
            self.rpc_client.clone().unwrap(), 
            mint, 
            pump_program
        ).await?;
        
        // Calculate price using the virtual reserves
        let virtual_sol_reserves = bonding_curve_reserves.virtual_sol_reserves as f64;
        let virtual_token_reserves = bonding_curve_reserves.virtual_token_reserves as f64;
        
        // Price formula: virtual_sol_reserves / virtual_token_reserves
        // Convert to a reasonable scale
        let price = virtual_sol_reserves / virtual_token_reserves;
        
        Ok(price)
    }

    // Update the build_swap_from_parsed_data method
    pub async fn build_swap_from_parsed_data(
        &self,
        trade_info: &crate::engine::transaction_parser::TradeInfoFromToken,
        swap_config: SwapConfig,
    ) -> Result<(Arc<Keypair>, Vec<Instruction>, f64)> {
        let started_time = Instant::now();
        let _logger = Logger::new("[PUMPFUN-SWAP-FROM-PARSED] => ".blue().to_string());
        _logger.log(format!("Building PumpFun swap from parsed transaction data"));
        
        // Basic validation - ensure we have a PumpFun transaction
        if trade_info.dex_type != crate::engine::transaction_parser::DexType::PumpFun {
            return Err(anyhow!("Invalid transaction type, expected PumpFun"));
        }
        
        // Extract the essential data
        let mint_str = &trade_info.mint;
        let owner = self.keypair.pubkey();
        let token_program_id = Pubkey::from_str(TOKEN_PROGRAM)?;
        let native_mint = spl_token::native_mint::ID;
        let pump_program = Pubkey::from_str(PUMP_PROGRAM)?;

        // Get bonding curve info
        let bonding_curve_info = if let Some(bonding_curve_info) = &trade_info.bonding_curve_info {
            Ok(get_bonding_curve_account_by_calc(bonding_curve_info.clone(), Pubkey::from_str(mint_str)?))
        } else {
            // If we don't have bonding curve info from parsed data, fetch it
            _logger.log("No bonding curve info in parsed data, fetching from RPC".to_string());
            get_bonding_curve_account(self.rpc_client.clone().unwrap(), Pubkey::from_str(mint_str)?, pump_program).await
        }?;

        // Unwrap results
        let (bonding_curve, associated_bonding_curve, _bonding_curve_reserves) = bonding_curve_info;

        // Determine if this is a buy or sell operation
        let (token_in, token_out, pump_method) = match swap_config.swap_direction {
            SwapDirection::Buy => (native_mint, Pubkey::from_str(mint_str)?, PUMP_BUY_METHOD),
            SwapDirection::Sell => (Pubkey::from_str(mint_str)?, native_mint, PUMP_SELL_METHOD),
        };
        // Get virtual reserves from parsed data or use defaults
        let virtual_sol_reserves = trade_info.virtual_sol_reserves.unwrap_or(INITIAL_VIRTUAL_SOL_RESERVES);
        let virtual_token_reserves = trade_info.virtual_token_reserves.unwrap_or(INITIAL_VIRTUAL_TOKEN_RESERVES);
        // Use slippage directly as basis points (already u64)
        let slippage_bps = swap_config.slippage;
        // Create instructions as needed
        let mut create_instruction = None;
        let mut close_instruction = None;
        // Handle token accounts based on direction (buy or sell)
        let in_ata = get_associated_token_address(&owner, &token_in);
        let out_ata = get_associated_token_address(&owner, &token_out);
        // Check if accounts exist and create if needed
        if swap_config.swap_direction == SwapDirection::Buy {
            // Check if token account exists using cache first
            if !self.check_token_account_cache(out_ata).await {
                create_instruction = Some(create_associated_token_account(
                    &owner,
                    &owner,
                    &token_out,
                    &token_program_id,
                ));
                // Cache the new account
                self.cache_token_account(out_ata).await;
            }
        } else {
            // For sell, check if we have tokens to sell using cache first
            if !self.check_token_account_cache(in_ata).await {
                return Err(anyhow!("Token ATA does not exist, cannot sell"));
            }
            
            // For sell transactions, determine if it's a full sell
            if swap_config.in_type == SwapInType::Pct && swap_config.amount_in >= 1.0 {
                // Close ATA for full sells
                close_instruction = Some(spl_token::instruction::close_account(
                    &token_program_id,
                    &in_ata,
                    &owner,
                    &owner,
                    &[&owner],
                )?);
            }
        }
        let coin_creator = match &trade_info.coin_creator {
            Some(creator) => Pubkey::from_str(creator).unwrap_or_else(|_| panic!("Invalid creator pubkey: {}", creator)),
            None => return Err(anyhow!("Coin creator not found in trade info")),
        };
        let (creator_vault, _) = Pubkey::find_program_address(
            &[b"creator-vault", coin_creator.as_ref()],
            &pump_program,
        );
        // Try to use parsed max_sol_cost or calculate from 
        //TODO: add rate copy 
        //TODO: currently fixed copy from token_amount in env
        let _is_use_copy_rate=false;
        //let copy_rate=10; // copy percent

        // Calculate token amount and threshold based on operation type and parsed data
        let (token_amount, sol_amount_threshold, input_accounts) = match swap_config.swap_direction {
            SwapDirection::Buy => {
                // if is_use_copy_rate {
                    // let sol_amount = if let Some(max_cost) = trade_info.max_sol_cost {
                    //     max_cost // Use parsed value directly
                    // } else {
                    //     // Calculate from swap_config
                    //     ui_amount_to_amount(swap_config.amount_in, spl_token::native_mint::DECIMALS)
                    // let available_sol = sol_amount;
                    // let available_sol_f64 = available_sol as f64;
    
                    // // Calculate using virtual reserves
                    // let virtual_sol_reserves_f64 = virtual_sol_reserves as f64;
                    // let virtual_token_reserves_f64 = virtual_token_reserves as f64;
                    // // Calculate constant product k
                    // let k_f64 = virtual_sol_reserves_f64 * virtual_token_reserves_f64;
                    // // Calculate new virtual SOL amount
                    // let new_virtual_sol_f64 = virtual_sol_reserves_f64 + available_sol_f64;
                    // // Calculate new virtual token amount
                    // let new_virtual_tokens_f64 = k_f64 / new_virtual_sol_f64;
                    // // Calculate expected tokens out
                    // let tokens_out_f64 = virtual_token_reserves_f64 - new_virtual_tokens_f64;
                    // // Apply slippage and round down
                    // let slippage_factor = 1.0 - (slippage_bps as f64 / 10000.0);
                    // let tokens_with_slippage = tokens_out_f64 * slippage_factor;
                    // // Use parsed base_amount_out if available, otherwise calculate
                    // let min_token_output = if let Some(amount_out) = trade_info.base_amount_out {
                    //     amount_out
                    // } else {
                    //     tokens_with_slippage.floor() as u64
                    // };
                    // };
                // }
                // Calculate expected token output using the virtual reserves
                print!("slippage_bps:::: {:?}", slippage_bps);
                let amount_specified = ui_amount_to_amount(swap_config.amount_in, spl_token::native_mint::DECIMALS);
                let max_sol_cost = max_amount_with_slippage(amount_specified, slippage_bps);
                let amount_result = u128::from(amount_specified)
                    .checked_mul(virtual_token_reserves as u128)
                    .expect("Failed to multiply amount_specified by virtual_token_reserves: overflow occurred.")
                    .checked_div(virtual_sol_reserves as u128)
                    .expect("Failed to divide the result by virtual_sol_reserves: division by zero or overflow occurred.");
                (
                    amount_result as u64,
                    max_sol_cost,
                    vec![
                        AccountMeta::new_readonly(Pubkey::from_str(PUMP_GLOBAL)?, false),   
                        AccountMeta::new(Pubkey::from_str(PUMP_FEE_RECIPIENT)?, false),
                        AccountMeta::new_readonly(Pubkey::from_str(mint_str)?, false),
                        AccountMeta::new(bonding_curve, false),
                        AccountMeta::new(associated_bonding_curve, false),
                        AccountMeta::new(out_ata, false),
                        AccountMeta::new(owner, true),
                        AccountMeta::new_readonly(system_program::id(), false),
                        AccountMeta::new_readonly(token_program_id, false),
                        AccountMeta::new(creator_vault, false),
                        AccountMeta::new_readonly(Pubkey::from_str(PUMP_EVENT_AUTHORITY)?, false),
                        AccountMeta::new_readonly(pump_program, false),
                    ]
                )
            },
            SwapDirection::Sell => {
                // Get token balance to sell - use trade_info.token_amount if available
                //TODO: let amount = if let Some(token_amount) = trade_info.token_amount {
                    //TODO:     token_amount
                    //TODO: } else {
                        // If not available in trade_info, get from account (fallback)
                    let amount = {
                    let in_account = token::get_account_info(
                        self.rpc_nonblocking_client.clone(),
                        token_in,
                        in_ata,
                    ).await?;
                    
                    let in_mint = token::get_mint_info(
                        self.rpc_nonblocking_client.clone(),
                        self.keypair.clone(),
                        token_in,
                    ).await?;
                    
                    // Calculate from swap_config
                    match swap_config.in_type {
                        SwapInType::Qty => {
                            ui_amount_to_amount(swap_config.amount_in, in_mint.base.decimals)
                        },
                        SwapInType::Pct => {
                            let amount_in_pct = swap_config.amount_in.min(1.0);
                            if amount_in_pct == 1.0 {
                                in_account.base.amount
                            } else {
                                (amount_in_pct * 100.0) as u64 * in_account.base.amount / 100
                            }
                        }
                    }
                 };
                
                // Validate amount
                if amount == 0 {
                    return Err(anyhow!("Amount is zero, cannot sell"));
                }
                
                // Calculate expected SOL output using virtual reserves
                let virtual_sol_reserves_f64 = virtual_sol_reserves as f64;
                let virtual_token_reserves_f64 = virtual_token_reserves as f64;
                let amount_f64 = amount as f64;
                
                // Calculate constant product k
                let k_f64 = virtual_sol_reserves_f64 * virtual_token_reserves_f64;
                
                // Calculate new virtual token amount after sell
                let new_virtual_tokens_f64 = virtual_token_reserves_f64 + amount_f64;
                
                // Calculate new virtual SOL amount
                let new_virtual_sol_f64 = k_f64 / new_virtual_tokens_f64;
                
                // Calculate expected SOL out
                let sol_out_f64 = virtual_sol_reserves_f64 - new_virtual_sol_f64;
                
                // Apply slippage and round down
                let slippage_factor = 1.0 - (slippage_bps as f64 / 10000.0);
                let sol_with_slippage = sol_out_f64 * slippage_factor;
                
                // Use parsed min_sol_output if available, otherwise calculate
                let min_sol_output = if let Some(output) = trade_info.min_sol_output {
                    output
                } else {
                    sol_with_slippage.floor() as u64
                };
                
                // Return accounts for sell
                (
                    amount,
                    min_sol_output,
                    vec![
                        AccountMeta::new_readonly(Pubkey::from_str(PUMP_GLOBAL)?, false),
                        AccountMeta::new(Pubkey::from_str(PUMP_FEE_RECIPIENT)?, false),
                        AccountMeta::new_readonly(Pubkey::from_str(mint_str)?, false),
                        AccountMeta::new(bonding_curve, false),
                        AccountMeta::new(associated_bonding_curve, false),
                        AccountMeta::new(in_ata, false),
                        AccountMeta::new(owner, true),
                        AccountMeta::new_readonly(system_program::id(), false),
                        AccountMeta::new(creator_vault, false),
                        AccountMeta::new_readonly(token_program_id, false),
                        AccountMeta::new_readonly(Pubkey::from_str(PUMP_EVENT_AUTHORITY)?, false),
                        AccountMeta::new_readonly(pump_program, false),
                    ]
                )
            }
        };

        // Build swap instruction
        let swap_instruction = Instruction::new_with_bincode(
            pump_program,
            &(pump_method, token_amount, sol_amount_threshold),
            input_accounts,
        );
        
        // Combine all instructions
        let mut instructions = vec![];
        if let Some(create_instruction) = create_instruction {
            instructions.push(create_instruction);
        }
        if token_amount > 0 {
            instructions.push(swap_instruction);
        }
        if let Some(close_instruction) = close_instruction {
            instructions.push(close_instruction);
        }
        
        // Validate we have instructions
        if instructions.is_empty() {
            return Err(anyhow!("Instructions is empty, no txn required."));
        }
        
        // Calculate token price
        let token_price = virtual_sol_reserves as f64 / virtual_token_reserves as f64;
        println!("time taken for build_swap_from_parsed_data: {:?}", started_time.elapsed());
        // Return the keypair, instructions, and the token price
        Ok((self.keypair.clone(), instructions, token_price))
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaydiumInfo {
    pub base: f64,
    pub quote: f64,
    pub price: f64,
}
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PumpInfo {
    pub mint: String,
    pub bonding_curve: String,
    pub associated_bonding_curve: String,
    pub raydium_pool: Option<String>,
    pub raydium_info: Option<RaydiumInfo>,
    pub complete: bool,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub total_supply: u64,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
pub struct BondingCurveAccount {
    pub discriminator: u64,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
}

#[derive(Debug, BorshSerialize, BorshDeserialize)]
pub struct BondingCurveReserves {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
}

pub fn get_bonding_curve_account_by_calc(
    bonding_curve_info: BondingCurveInfo,
    mint: Pubkey,
) -> (Pubkey, Pubkey, BondingCurveReserves) {
    let bonding_curve = bonding_curve_info.bonding_curve;
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, &mint);
    
    let bonding_curve_reserves = BondingCurveReserves 
        { 
            virtual_token_reserves: bonding_curve_info.new_virtual_token_reserve, 
            virtual_sol_reserves: bonding_curve_info.new_virtual_sol_reserve,
        };

    (
        bonding_curve,
        associated_bonding_curve,
        bonding_curve_reserves,
    )
}

pub async fn get_bonding_curve_account(
    rpc_client: Arc<anchor_client::solana_client::rpc_client::RpcClient>,
    mint: Pubkey,
    pump_program: Pubkey,
) -> Result<(Pubkey, Pubkey, BondingCurveReserves)> {
    let bonding_curve = get_pda(&mint, &pump_program)?;
    let associated_bonding_curve = get_associated_token_address(&bonding_curve, &mint);
    
    // Get account data and token balance sequentially since RpcClient is synchronous
    let bonding_curve_data_result = rpc_client.get_account_data(&bonding_curve);
    let token_balance_result = rpc_client.get_token_account_balance(&associated_bonding_curve);
    
    let bonding_curve_reserves = match bonding_curve_data_result {
        Ok(bonding_curve_data) => {
            match from_slice::<BondingCurveAccount>(&bonding_curve_data) {
                Ok(bonding_curve_account) => BondingCurveReserves {
                    virtual_token_reserves: bonding_curve_account.virtual_token_reserves,
                    virtual_sol_reserves: bonding_curve_account.virtual_sol_reserves 
                },
                Err(_) => {
                    // Fallback to direct balance checks
                    let bonding_curve_sol_balance = rpc_client.get_balance(&bonding_curve).unwrap_or(0);
                    let token_balance = match &token_balance_result {
                        Ok(balance) => {
                            match balance.ui_amount {
                                Some(amount) => (amount * (10f64.powf(balance.decimals as f64))) as u64,
                                None => 0,
                            }
                        },
                        Err(_) => 0
                    };
                    
                    BondingCurveReserves {
                        virtual_token_reserves: token_balance,
                        virtual_sol_reserves: bonding_curve_sol_balance,
                    }
                }
            }
        },
        Err(_) => {
            // Fallback to direct balance checks
            let bonding_curve_sol_balance = rpc_client.get_balance(&bonding_curve).unwrap_or(0);
            let token_balance = match &token_balance_result {
                Ok(balance) => {
                    match balance.ui_amount {
                        Some(amount) => (amount * (10f64.powf(balance.decimals as f64))) as u64,
                        None => 0,
                    }
                },
                Err(_) => 0
            };
            
            BondingCurveReserves {
                virtual_token_reserves: token_balance,
                virtual_sol_reserves: bonding_curve_sol_balance,
            }
        }
    };

    Ok((
        bonding_curve,
        associated_bonding_curve,
        bonding_curve_reserves,
    ))
}

fn max_amount_with_slippage(input_amount: u64, slippage_bps: u64) -> u64 {
    input_amount
        .checked_mul(slippage_bps.checked_add(TEN_THOUSAND).unwrap())
        .unwrap()
        .checked_div(TEN_THOUSAND)
        .unwrap()
}

pub fn get_pda(mint: &Pubkey, program_id: &Pubkey ) -> Result<Pubkey> {
    let seeds = [b"bonding-curve".as_ref(), mint.as_ref()];
    let (bonding_curve, _bump) = Pubkey::find_program_address(&seeds, program_id);
    Ok(bonding_curve)
}

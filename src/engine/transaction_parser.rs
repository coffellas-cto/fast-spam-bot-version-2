use bs58;
use std::str::FromStr;
use solana_sdk::pubkey::Pubkey;
use colored::Colorize;
use crate::common::logger::Logger;
use lazy_static;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use std::time::Instant;
use crate::dex::pump_fun::PUMP_PROGRAM;
// Create a static logger for this module
lazy_static::lazy_static! {
    static ref LOGGER: Logger = Logger::new("[TRANSACTION-PARSER] => ".blue().to_string());
}

#[derive(Clone, Debug, PartialEq)]
pub enum DexType {
    PumpSwap,
    PumpFun,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct ParsedData {
    pub sol_change: f64,
    pub token_change: f64,
    pub is_buy: bool,
    pub user: String,
    pub mint: Option<String>,
    pub timestamp: Option<u64>,
    pub real_sol_reserves: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PumpSwapData {
    pub timestamp: u64,
    pub base_amount_in: u64,
    pub min_quote_amount_out: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub quote_amount_out: u64,
    pub lp_fee_basis_points: u64,
    pub lp_fee: u64,
    pub protocol_fee_basis_points: u64,
    pub protocol_fee: u64,
    pub quote_amount_out_without_lp_fee: u64,
    pub user_quote_amount_out: u64,
    pub pool: String,
    pub user: String,
    pub target_user_base_token_account: String,
    pub target_user_quote_token_account: String,
    pub protocol_fee_recipient: String,
    pub protocol_fee_recipient_token_account: String,
}

#[derive(Clone, Debug)]
pub struct PumpFunData {
    pub mint: String,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: String,
    pub timestamp: u64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
}

#[derive(Clone, Debug)]
pub struct TransactionOutput {
    pub sol_changes: f64,
    pub token_changes: f64,
    pub is_buy: bool,
    pub user: String,
    pub instruction_type: DexType,
    pub timestamp: u64,
    pub mint: String,
    pub signature: String,
}

#[derive(Clone, Debug)]
pub struct TradeInfoFromToken {
    // Common fields
    pub dex_type: DexType,
    pub slot: u64,
    pub signature: String,
    pub target: String,
    // Fields from both PumpSwapData and PumpFunData
    pub mint: String,
    pub user: String,
    pub timestamp: u64,
    pub is_buy: bool,
    // PumpSwapData fields
    pub base_amount_in: Option<u64>,
    pub min_quote_amount_out: Option<u64>,
    pub user_base_token_reserves: Option<u64>,
    pub user_quote_token_reserves: Option<u64>,
    pub pool_base_token_reserves: Option<u64>,
    pub pool_quote_token_reserves: Option<u64>,
    pub quote_amount_out: Option<u64>, //  quote_amount_out in case of sell , quote_amount_in in case of buy
    pub lp_fee_basis_points: Option<u64>,
    pub lp_fee: Option<u64>,
    pub protocol_fee_basis_points: Option<u64>,
    pub protocol_fee: Option<u64>,
    pub quote_amount_out_without_lp_fee: Option<u64>,
    pub user_quote_amount_out: Option<u64>, // user_quote_amount_out in case of sell , user_base_amount_in in case of buy
    pub pool: Option<String>,
    pub user_base_token_account: Option<String>,
    pub user_quote_token_account: Option<String>,
    pub protocol_fee_recipient: Option<String>,
    pub protocol_fee_recipient_token_account: Option<String>,
    pub coin_creator: Option<String>,
    pub coin_creator_fee_basis_points: Option<u64>,
    pub coin_creator_fee: Option<u64>,
    
    // PumpFunData fields
    pub sol_amount: Option<u64>,
    pub token_amount: Option<u64>,
    pub virtual_sol_reserves: Option<u64>,
    pub virtual_token_reserves: Option<u64>,
    pub real_sol_reserves: Option<u64>,
    pub real_token_reserves: Option<u64>,
    
    // Additional fields from original TradeInfoFromToken
    pub bonding_curve: String,
    pub volume_change: i64,
    pub bonding_curve_info: Option<crate::engine::monitor::BondingCurveInfo>,
    pub pool_info: Option<crate::engine::monitor::PoolInfo>,
    pub token_amount_f64: f64,
    pub amount: Option<u64>,
    pub max_sol_cost: Option<u64>,
    pub min_sol_output: Option<u64>,
    pub base_amount_out: Option<u64>,
    pub max_quote_amount_in: Option<u64>,
}
/// Parses the transaction data buffer into a TradeInfoFromToken struct
pub fn parse_transaction_data(txn: &SubscribeUpdateTransaction, buffer: &[u8]) -> Option<TradeInfoFromToken> {
    fn parse_public_key(buffer: &[u8], offset: usize) -> Option<String> {
        if offset + 32 > buffer.len() {
            return None;
        }
        Some(bs58::encode(&buffer[offset..offset+32]).into_string())
    }

    fn parse_u64(buffer: &[u8], offset: usize) -> Option<u64> {
        if offset + 8 > buffer.len() {
            return None;
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buffer[offset..offset+8]);
        Some(u64::from_le_bytes(bytes))
    }
    
    // Helper function to extract token mint from token balances
    fn extract_token_info(
        txn: &SubscribeUpdateTransaction,
    ) -> Option<String> {
        
        let mut mint = String::new();
        // Try to extract from token balances if txn is available
        if let Some(tx_inner) = &txn.transaction {
            if let Some(meta) = &tx_inner.meta {
                // Check post token balances
                if !meta.post_token_balances.is_empty() {
                    mint = meta.post_token_balances[0].mint.clone();
                }
            }
        }
        // If we couldn't extract from token balances, use default
        if mint.is_empty() {
            mint = "2ivzYvjnKqA4X3dVvPKr7bctGpbxwrXbbxm44TJCpump".to_string();
        }
        
        Some(mint)
    }

    // Helper function to calculate volume change for PumpSwap (368 bytes)
    fn calculate_pumpswap_volume_change(txn: &SubscribeUpdateTransaction, pool: &str) -> i64 {
        let account_keys = match &txn.transaction {
            Some(tx) => match &tx.transaction {
                Some(transaction) => match &transaction.message {
                    Some(msg) => &msg.account_keys,
                    None => return 0,
                },
                None => return 0,
            },
            None => return 0,
        };
        
        let pool_index = account_keys
            .iter()
            .position(|account_key| {
                match Pubkey::try_from(account_key.clone()) {
                    Ok(pubkey) => pubkey.to_string() == *pool,
                    Err(_) => false,
                }
            })
            .unwrap_or(0);

        if let Some(meta) = &txn.transaction.as_ref().and_then(|t| t.meta.as_ref()) {
            let sol_post_amount = *meta.post_balances.get(pool_index).unwrap_or(&0_u64);
            let sol_pre_amount = *meta.pre_balances.get(pool_index).unwrap_or(&0_u64);
            return (sol_post_amount as i64) - (sol_pre_amount as i64);
        }
        
        0
    }

    // Helper function to calculate volume change for PumpFun (233 bytes)
    fn calculate_pumpfun_volume_change(txn: &SubscribeUpdateTransaction, bonding_curve: &str) -> i64 {
        let account_keys = match &txn.transaction {
            Some(tx) => match &tx.transaction {
                Some(transaction) => match &transaction.message {
                    Some(msg) => &msg.account_keys,
                    None => return 0,
                },
                None => return 0,
            },
            None => return 0,
        };
        
        let bonding_curve_index = account_keys
            .iter()
            .position(|account_key| {
                match Pubkey::try_from(account_key.clone()) {
                    Ok(pubkey) => pubkey.to_string() == *bonding_curve,
                    Err(_) => false,
                }
            })
            .unwrap_or(0);

        if let Some(meta) = &txn.transaction.as_ref().and_then(|t| t.meta.as_ref()) {
            let sol_post_amount = *meta.post_balances.get(bonding_curve_index).unwrap_or(&0_u64);
            let sol_pre_amount = *meta.pre_balances.get(bonding_curve_index).unwrap_or(&0_u64);
            return (sol_post_amount as i64) - (sol_pre_amount as i64);
        }
        
        0
    }

    let start_time = Instant::now();
    match buffer.len() {
        368 => {
            // Extract token mint - pass None for the txn since we don't have it here
            let mint = extract_token_info(&txn).unwrap_or_else(|| 
                "2ivzYvjnKqA4X3dVvPKr7bctGpbxwrXbbxm44TJCpump".to_string()
            );
            let timestamp = parse_u64(buffer, 16)?;
            let base_amount_in = parse_u64(buffer, 24)?;
            let min_quote_amount_out = parse_u64(buffer, 32)?;
            let user_base_token_reserves = parse_u64(buffer, 40)?;
            let user_quote_token_reserves = parse_u64(buffer, 48)?;
            let pool_base_token_reserves = parse_u64(buffer, 56)?;
            let pool_quote_token_reserves = parse_u64(buffer, 64)?;
            let quote_amount_out = parse_u64(buffer, 72)?;
            let lp_fee_basis_points = parse_u64(buffer, 80)?;
            let lp_fee = parse_u64(buffer, 88)?;
            let protocol_fee_basis_points = parse_u64(buffer, 96)?;
            let protocol_fee = parse_u64(buffer, 104)?;
            let quote_amount_out_without_lp_fee = parse_u64(buffer, 112)?;
            let user_quote_amount_out = parse_u64(buffer, 120)?;
            let pool = parse_public_key(buffer, 128)?;
            let user = parse_public_key(buffer, 160)?;
            let user_base_token_account = parse_public_key(buffer, 192)?;
            let user_quote_token_account = parse_public_key(buffer, 224)?;
            let protocol_fee_recipient = parse_public_key(buffer, 256)?;
            let protocol_fee_recipient_token_account = parse_public_key(buffer, 288)?;
            let coin_creator = parse_public_key(buffer, 320)?;
            let coin_creator_fee_basis_points = parse_u64(buffer, 328)?;
            let coin_creator_fee = parse_u64(buffer, 336)?;
            let is_buy = quote_amount_out_without_lp_fee > quote_amount_out;
            
            // Calculate volume change for PumpSwap using pool account
            let volume_change = calculate_pumpswap_volume_change(txn, &pool);
            
            let pool_info = crate::engine::monitor::PoolInfo {
                pool_id: Pubkey::from_str(&pool).unwrap_or_else(|_| panic!("Invalid pool pubkey: {}", pool)),
                base_mint: Pubkey::from_str(&mint).unwrap_or_else(|_| panic!("Invalid mint pubkey: {}", mint)),
                quote_mint: Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap_or_else(|_| panic!("Invalid WSOL mint pubkey")),
                base_reserve: pool_base_token_reserves,
                quote_reserve: pool_quote_token_reserves,
                coin_creator: Pubkey::from_str(&coin_creator).unwrap_or_else(|_| panic!("Invalid creator pubkey: {}", coin_creator)),
            };

            println!("parsing time: {:?}", start_time.elapsed());
            Some(TradeInfoFromToken {
                dex_type: DexType::PumpSwap,
                slot: 0, // Will be set from transaction data
                signature: String::new(), // Will be set from transaction data
                target: String::new(), // Will be set from transaction data
                mint, // Will be set from pool data
                user,
                timestamp,
                is_buy,
                // PumpSwapData fields
                base_amount_in: Some(base_amount_in),
                min_quote_amount_out: Some(min_quote_amount_out),
                user_base_token_reserves: Some(user_base_token_reserves),
                user_quote_token_reserves: Some(user_quote_token_reserves),
                pool_base_token_reserves: Some(pool_base_token_reserves),
                pool_quote_token_reserves: Some(pool_quote_token_reserves),
                quote_amount_out: Some(quote_amount_out),
                lp_fee_basis_points: Some(lp_fee_basis_points),
                lp_fee: Some(lp_fee),
                protocol_fee_basis_points: Some(protocol_fee_basis_points),
                protocol_fee: Some(protocol_fee),
                quote_amount_out_without_lp_fee: Some(quote_amount_out_without_lp_fee),
                user_quote_amount_out: Some(user_quote_amount_out),
                pool: Some(pool),
                user_base_token_account: Some(user_base_token_account),
                user_quote_token_account: Some(user_quote_token_account),
                protocol_fee_recipient: Some(protocol_fee_recipient),
                protocol_fee_recipient_token_account: Some(protocol_fee_recipient_token_account),
                coin_creator: Some(coin_creator),
                coin_creator_fee_basis_points: Some(coin_creator_fee_basis_points),
                coin_creator_fee: Some(coin_creator_fee),

                // PumpFunData fields - None
                sol_amount: None,
                token_amount: None,
                virtual_sol_reserves: None,
                virtual_token_reserves: None,
                real_sol_reserves: None,
                real_token_reserves: None,
                
                // Additional fields
                bonding_curve: String::new(),
                volume_change,
                bonding_curve_info: None,
                pool_info: Some(pool_info),
                token_amount_f64: base_amount_in as f64,
                amount: None,
                max_sol_cost: None,
                min_sol_output: None,
                base_amount_out: None,
                max_quote_amount_in: None,
            })
        },

        233 => {
            // Parse PumpFunData fields
            let mint = parse_public_key(buffer, 16)?;
            let sol_amount = parse_u64(buffer, 48)?;
            let token_amount = parse_u64(buffer, 56)?;
            let is_buy = buffer.get(64)? == &1;
            let user = parse_public_key(buffer, 65)?;
            let timestamp = parse_u64(buffer, 97)?;
            let virtual_sol_reserves = parse_u64(buffer, 105)?;
            let virtual_token_reserves = parse_u64(buffer, 113)?;
            let real_sol_reserves = parse_u64(buffer, 121)?;
            let real_token_reserves = parse_u64(buffer, 129)?;
            let _fee_recipient = parse_public_key(buffer, 137)?;
            let _fee_basis_points = parse_u64(buffer, 169)?;
            let _fee = parse_u64(buffer, 177)?;
            let creator = parse_public_key(buffer, 185)?;
            let creator_fee_basis_points = parse_u64(buffer, 217)?;
            let creator_fee = parse_u64(buffer, 225)?;

            let bonding_curve = crate::dex::pump_fun::get_pda(
                &Pubkey::from_str(&mint).unwrap_or_else(|_| panic!("Invalid mint pubkey: {}", mint)), 
                &Pubkey::from_str(PUMP_PROGRAM).unwrap_or_else(|_| panic!("Invalid pump program pubkey: {}", PUMP_PROGRAM))
            ).ok()?;
            
            // Calculate volume change for PumpFun using bonding curve account
            let volume_change = calculate_pumpfun_volume_change(txn, &bonding_curve.to_string());
            
            let bonding_curve_info = crate::engine::monitor::BondingCurveInfo {    
                bonding_curve: bonding_curve.clone(),
                new_virtual_sol_reserve: virtual_sol_reserves,
                new_virtual_token_reserve: virtual_token_reserves,
            };

            // Pump fun don't have pool, just have bonding curve
            // so we need to create a pool info for selling logic to work
            let pool_info = crate::engine::monitor::PoolInfo {
                pool_id: Pubkey::default(), // Use default Pubkey instead of None
                base_mint: Pubkey::from_str(&mint).unwrap_or_else(|_| panic!("Invalid mint pubkey: {}", mint)),
                quote_mint: Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap_or_else(|_| panic!("Invalid WSOL mint pubkey")),
                base_reserve: real_token_reserves,
                quote_reserve: real_sol_reserves,
                coin_creator: Pubkey::from_str(&creator).unwrap_or_else(|_| panic!("Invalid creator pubkey: {}", creator)),
            };
            
            Some(TradeInfoFromToken {
                dex_type: DexType::PumpFun,
                slot: 0, // Will be set from transaction data
                signature: String::new(), // Will be set from transaction data
                target: String::new(), // Will be set from transaction data
                mint,
                user,
                timestamp,
                is_buy,
                // PumpSwapData fields - None
                base_amount_in: None,
                min_quote_amount_out: None,
                user_base_token_reserves: None,
                user_quote_token_reserves: None,
                pool_base_token_reserves: None,
                pool_quote_token_reserves: None,
                quote_amount_out: None,
                lp_fee_basis_points: None,
                lp_fee: None,
                protocol_fee_basis_points: None,
                protocol_fee: None,
                quote_amount_out_without_lp_fee: None,
                user_quote_amount_out: None,
                pool: None,
                user_base_token_account: None,
                user_quote_token_account: None,
                protocol_fee_recipient: None,
                protocol_fee_recipient_token_account: None,
                coin_creator: Some(creator),
                coin_creator_fee_basis_points: Some(creator_fee_basis_points),
                coin_creator_fee: Some(creator_fee),
                
                // PumpFunData fields
                sol_amount: Some(sol_amount),
                token_amount: Some(token_amount),
                virtual_sol_reserves: Some(virtual_sol_reserves),
                virtual_token_reserves: Some(virtual_token_reserves),
                real_sol_reserves: Some(real_sol_reserves),
                real_token_reserves: Some(real_token_reserves),
                
                // Additional fields
                bonding_curve: String::new(),
                volume_change,
                bonding_curve_info: Some(bonding_curve_info),
                pool_info: Some(pool_info),
                token_amount_f64: token_amount as f64,
                amount: None,
                max_sol_cost: None,
                min_sol_output: None,
                base_amount_out: None,
                max_quote_amount_in: None,
            })
        },
        _ => None,
    }
}

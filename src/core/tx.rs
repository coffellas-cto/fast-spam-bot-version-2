use std::sync::Arc;
use std::str::FromStr;
use anyhow::Result;
use colored::Colorize;
use anchor_client::solana_sdk::{
    instruction::Instruction,
    signature::Keypair,
    system_instruction,
    transaction::Transaction,
    hash::Hash,
    signature::Signature,
};
use anchor_client::solana_sdk::pubkey::Pubkey;
use spl_token::ui_amount_to_amount;
use solana_sdk::signer::Signer;
use tokio::time::{Instant, sleep};
use once_cell::sync::Lazy;
use reqwest::Client;
use base64;
use std::time::Duration;
use std::env;
use crate::{
    common::logger::Logger,
};
use dotenv::dotenv;
use crate::services::nozomi::get_tip_account;

// Cache the tip value for better performance
static NOZOMI_TIP_VALUE: Lazy<f64> = Lazy::new(|| {
    std::env::var("NOZOMI_TIP_VALUE")
        .ok()
        .and_then(|v| f64::from_str(&v).ok())
        .unwrap_or(0.0015)
});

// Cache the FlashBlock API key
static FLASHBLOCK_API_KEY: Lazy<String> = Lazy::new(|| {
    std::env::var("FLASHBLOCK_API_KEY")
        .ok()
        .unwrap_or_else(|| "da07907679634859".to_string())
});

// Create a static HTTP client with optimized configuration for FlashBlock API
static HTTP_CLIENT: Lazy<Client> = Lazy::new(|| {
   let client = reqwest::Client::new();
   client
});

// Get nozomi tip value from env
pub fn get_nozomi_tip() -> f64 {
    dotenv().ok();
    std::env::var("NOZOMI_TIP_VALUE")
        .ok()
        .and_then(|v| f64::from_str(&v).ok())
        .unwrap_or(0.0015)
}

// prioritization fee = UNIT_PRICE * UNIT_LIMIT
fn get_unit_price() -> u64 {
    env::var("UNIT_PRICE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20000)
}

fn get_unit_limit() -> u32 {
    env::var("UNIT_LIMIT")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(200_000)
}

/// Send a signed transaction using the standard RPC client
pub async fn new_signed_and_send(
    rpc_client: Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    recent_blockhash: Hash,
    keypair: &Keypair,
    instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<Signature>, String> {
    // Create transaction
    let mut tx = Transaction::new_with_payer(&instructions, Some(&keypair.pubkey()));
    tx.sign(&[keypair], recent_blockhash);
    
    // send init txn
    let _txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        recent_blockhash,
    );

    // Send transaction
    match rpc_client.send_and_confirm_transaction_with_spinner(&tx).await {
        Ok(signature) => {
            logger.log(format!("Transaction sent successfully: {}", signature).green().to_string());
            Ok(vec![signature])
        },
        Err(e) => {
            logger.log(format!("Failed to send transaction: {}", e).red().to_string());
            Err(format!("Transaction failed: {}", e))
        }
    }
}

/// Send a signed transaction using Nozomi RPC for faster execution
pub async fn new_signed_and_send_flashblock(
    _rpc_client: Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    recent_blockhash: Hash,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<Signature>, String> {
    let start_time = Instant::now();
    
    let flashblock_tip_account = Pubkey::from_str("FLaShB3iXXTWE1vu9wQsChUKq3HFtpMAhb8kAh1pf1wi")
        .map_err(|e| format!("Invalid Flashblock tip account: {}", e))?;
    let flashblock_tip_instruction = system_instruction::transfer(&keypair.pubkey(), &flashblock_tip_account, 100000);
    
    // Add tip instruction and construct transaction
    instructions.insert(0, flashblock_tip_instruction);
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        recent_blockhash,
    );
    
    // Serialize and base64 encode the transaction
    let serialized_txn = bincode::serialize(&txn)
        .map_err(|e| format!("Failed to serialize transaction: {}", e))?;
    let base64_txn = base64::encode(&serialized_txn);

    // Retry logic for failed requests
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_MS: u64 = 500;
    let client = HTTP_CLIENT.clone();
    let mut last_error = None;
    for attempt in 0..MAX_RETRIES {

        println!("send flashblock txn");

        match client
            .post("http://ny.flashblock.trade/api/v2/submit-batch")
            .header("Content-Type", "application/json")
            .header("Authorization", FLASHBLOCK_API_KEY.as_str())
            .json(&serde_json::json!({
                "jsonrpc": "3.0",
                "id": 1,
                "method": "POST",
                "transactions": [base64_txn]
            }))
            .send()
            .await
        {
            Ok(response) => {
                match response.json::<serde_json::Value>().await {
                    Ok(result) => {
                        logger.log(
                            format!("[TXN-ELAPSED(FLASHBLOCK)]: {:?}", start_time.elapsed())
                                .yellow()
                                .to_string(),
                        );

                        // Check if the response indicates success
                        if result["success"].as_bool().unwrap_or(false) {
                            // Log the full response for debugging
                            logger.log(format!("FlashBlock response: {:?}", result).yellow().to_string());
                            
                            // Check for signatures in the response
                            if let Some(signatures) = result["data"]["signatures"].as_array() {
                                if !signatures.is_empty() {
                                    return Ok(signatures
                                        .iter()
                                        .filter_map(|sig| sig.as_str().map(|s| Signature::from_str(s).ok()).flatten())
                                        .collect());
                                } else {
                                    last_error = Some("Signatures array is empty".to_string());
                                }
                            } else {
                                // Log the actual response structure for debugging
                                logger.log(format!("Response structure: {:?}", result).yellow().to_string());
                                last_error = Some("No signatures found in response data".to_string());
                            }
                        } else {
                            let error_msg = result["message"].as_str().unwrap_or("Unknown error");
                            last_error = Some(format!("FlashBlock API error: {}", error_msg));
                        }
                    }
                    Err(e) => {
                        last_error = Some(format!("Failed to parse response: {}", e));
                    }
                }
            }
            Err(e) => {
                last_error = Some(format!("Request failed: {}", e));
            }
        }

        if attempt < MAX_RETRIES - 1 {
            logger.log(format!("Retrying FlashBlock request (attempt {}/{})", attempt + 1, MAX_RETRIES).yellow().to_string());
            sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| format!("Failed to send transaction after {} attempts", MAX_RETRIES)))
}

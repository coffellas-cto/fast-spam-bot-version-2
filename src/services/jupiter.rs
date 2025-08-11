use std::sync::Arc;
use std::str::FromStr;
use anyhow::{anyhow, Result};
use colored::Colorize;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use anchor_client::solana_sdk::{
    signature::{Keypair, Signer}, // Add Signer trait import
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use base64;
use tokio::time::Duration;

use crate::common::logger::Logger;

const JUPITER_API_URL: &str = "https://quote-api.jup.ag/v6";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Serialize)]
struct QuoteRequest {
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    amount: String,
    #[serde(rename = "slippageBps")]
    slippage_bps: u64,
}

#[derive(Debug, Deserialize, Serialize)] // Add Serialize derive
struct QuoteResponse {
    #[serde(rename = "inputMint")]
    pub input_mint: String,
    #[serde(rename = "inAmount")]
    pub in_amount: String,
    #[serde(rename = "outputMint")]
    pub output_mint: String,
    #[serde(rename = "outAmount")]
    pub out_amount: String,
    #[serde(rename = "otherAmountThreshold")]
    pub other_amount_threshold: String,
    #[serde(rename = "swapMode")]
    pub swap_mode: String,
    #[serde(rename = "slippageBps")]
    pub slippage_bps: u64,
    #[serde(rename = "priceImpactPct")]
    pub price_impact_pct: String,
}

#[derive(Debug, Serialize)]
struct SwapRequest {
    #[serde(rename = "quoteResponse")]
    quote_response: QuoteResponse,
    #[serde(rename = "userPublicKey")]
    user_public_key: String,
    #[serde(rename = "wrapAndUnwrapSol")]
    wrap_and_unwrap_sol: bool,
    #[serde(rename = "dynamicComputeUnitLimit")]
    dynamic_compute_unit_limit: bool,
    #[serde(rename = "prioritizationFeeLamports")]
    prioritization_fee_lamports: PrioritizationFee,
}

#[derive(Debug, Serialize)]
struct PrioritizationFee {
    #[serde(rename = "priorityLevelWithMaxLamports")]
    priority_level_with_max_lamports: PriorityLevel,
}

#[derive(Debug, Serialize)]
struct PriorityLevel {
    #[serde(rename = "maxLamports")]
    max_lamports: u64,
    #[serde(rename = "priorityLevel")]
    priority_level: String,
}

#[derive(Debug, Deserialize)]
struct SwapResponse {
    #[serde(rename = "swapTransaction")]
    pub swap_transaction: String,
}

#[derive(Clone)]
pub struct JupiterClient {
    client: Client,
    rpc_client: Arc<RpcClient>,
    logger: Logger,
}

impl JupiterClient {
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");
            
        Self {
            client,
            rpc_client,
            logger: Logger::new("[JUPITER] => ".magenta().to_string()),
        }
    }

    /// Get a quote for swapping tokens
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u64,
    ) -> Result<QuoteResponse> {
        self.logger.log(format!("Getting Jupiter quote: {} -> {} (amount: {}, slippage: {}bps)", 
            input_mint, output_mint, amount, slippage_bps));

        let quote_request = QuoteRequest {
            input_mint: input_mint.to_string(),
            output_mint: output_mint.to_string(),
            amount: amount.to_string(),
            slippage_bps,
        };

        let url = format!("{}/quote", JUPITER_API_URL);
        let response = self.client
            .get(&url)
            .query(&[
                ("inputMint", &quote_request.input_mint),
                ("outputMint", &quote_request.output_mint),
                ("amount", &quote_request.amount),
                ("slippageBps", &quote_request.slippage_bps.to_string()),
            ])
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!("Jupiter quote API error: {}", error_text));
        }

        let quote: QuoteResponse = response.json().await?;
        
        self.logger.log(format!("Jupiter quote received: {} {} -> {} {} (price impact: {}%)", 
            quote.in_amount, input_mint, quote.out_amount, output_mint, quote.price_impact_pct));

        Ok(quote)
    }

    /// Get swap transaction from Jupiter
    pub async fn get_swap_transaction(
        &self,
        quote: QuoteResponse,
        user_public_key: &Pubkey,
    ) -> Result<VersionedTransaction> {
        self.logger.log(format!("Getting Jupiter swap transaction for user: {}", user_public_key));

        let swap_request = SwapRequest {
            quote_response: quote,
            user_public_key: user_public_key.to_string(),
            wrap_and_unwrap_sol: true,
            dynamic_compute_unit_limit: true,
            prioritization_fee_lamports: PrioritizationFee {
                priority_level_with_max_lamports: PriorityLevel {
                    max_lamports: 1_000_000, // 0.001 SOL max priority fee
                    priority_level: "high".to_string(),
                },
            },
        };

        let url = format!("{}/swap", JUPITER_API_URL);
        let response = self.client
            .post(&url)
            .json(&swap_request)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow!("Jupiter swap API error: {}", error_text));
        }

        let swap_response: SwapResponse = response.json().await?;
        
        // Decode the base64 transaction
        let transaction_bytes = base64::decode(&swap_response.swap_transaction)?;
        let transaction: VersionedTransaction = bincode::deserialize(&transaction_bytes)?;

        self.logger.log("Jupiter swap transaction received and decoded successfully".to_string());

        Ok(transaction)
    }

    /// Execute a token sell using Jupiter (complete flow)
    pub async fn sell_token_with_jupiter(
        &self,
        token_mint: &str,
        token_amount: u64,
        slippage_bps: u64,
        keypair: &Keypair,
    ) -> Result<String> {
        self.logger.log(format!("Starting Jupiter sell for token {} (amount: {})", token_mint, token_amount));

        // Get quote
        let quote = self.get_quote(
            token_mint,
            SOL_MINT,
            token_amount,
            slippage_bps,
        ).await?;

        // Get swap transaction
        let mut transaction = self.get_swap_transaction(quote, &keypair.pubkey()).await?;

        // Get recent blockhash
        let recent_blockhash = self.rpc_client.get_latest_blockhash().await?;
        transaction.message.set_recent_blockhash(recent_blockhash);

        // Sign the transaction
        transaction.try_partial_sign(&[keypair], recent_blockhash)?;

        // Send the transaction
        let signature = self.rpc_client.send_transaction(&transaction).await?;

        self.logger.log(format!("Jupiter sell transaction sent: {}", signature).green().to_string());

        Ok(signature.to_string())
    }

    /// Verify if a transaction was successful
    pub async fn verify_transaction(&self, signature: &str) -> Result<bool> {
        let signature = anchor_client::solana_sdk::signature::Signature::from_str(signature)?;
        
        // Wait a bit for transaction to settle
        tokio::time::sleep(Duration::from_secs(2)).await;

        match self.rpc_client.get_signature_status(&signature).await? {
            Some(result) => {
                match result {
                    Ok(_) => {
                        self.logger.log(format!("Transaction {} confirmed successfully", signature).green().to_string());
                        Ok(true)
                    },
                    Err(e) => {
                        self.logger.log(format!("Transaction {} failed: {:?}", signature, e).red().to_string());
                        Ok(false)
                    }
                }
            },
            None => {
                self.logger.log(format!("Transaction {} not found or still pending", signature).yellow().to_string());
                Ok(false)
            }
        }
    }
} 
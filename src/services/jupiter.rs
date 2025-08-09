use std::collections::HashMap;
use std::str::FromStr;
use anyhow::{Result, anyhow};
use anchor_client::solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use reqwest::Client;
use serde_json::{json, Value};
use colored::Colorize;
use crate::common::logger::Logger;

// SOL mint address (wrapped SOL)
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug)]
pub struct JupiterClient {
    client: Client,
    api_url: String,
    logger: Logger,
}

impl JupiterClient {
    pub fn new() -> Self {
        let api_url = std::env::var("JUPITER_API_URL")
            .unwrap_or_else(|_| "https://quote-api.jup.ag/v6".to_string());
        
        Self {
            client: Client::new(),
            api_url,
            logger: Logger::new("[JUPITER] => ".yellow().to_string()),
        }
    }

    /// Get all token accounts for the wallet
    async fn get_wallet_token_accounts(&self, rpc_client: &RpcClient, wallet_pubkey: &Pubkey) -> Result<Vec<TokenAccount>> {
        let token_program = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")?;
        
        let accounts = rpc_client.get_token_accounts_by_owner(
            wallet_pubkey,
            anchor_client::solana_client::rpc_request::TokenAccountsFilter::ProgramId(token_program)
        ).await?;

        let mut token_accounts = Vec::new();
        
        for account in accounts {
            if let Ok(account_data) = rpc_client.get_account(&Pubkey::from_str(&account.pubkey)?).await {
                if let Ok(parsed) = spl_token::state::Account::unpack(&account_data.data) {
                    if parsed.amount > 0 && parsed.mint.to_string() != SOL_MINT {
                        // Get mint info to determine decimals
                        let mint_info = rpc_client.get_account(&parsed.mint).await?;
                        let mint_data = spl_token::state::Mint::unpack(&mint_info.data)?;
                        let decimals = mint_data.decimals;
                        
                        token_accounts.push(TokenAccount {
                            mint: parsed.mint.to_string(),
                            amount: parsed.amount,
                            decimals,
                            ui_amount: parsed.amount as f64 / 10_f64.powi(decimals as i32),
                        });
                    }
                }
            }
        }

        Ok(token_accounts)
    }

    /// Get token prices from Jupiter API
    async fn get_token_prices(&self, mints: &[String]) -> Result<HashMap<String, f64>> {
        if mints.is_empty() {
            return Ok(HashMap::new());
        }

        let mint_string = mints.join(",");
        let url = format!("{}/price?ids={}", self.api_url, mint_string);
        
        let response = self.client.get(&url).send().await?;
        let data: Value = response.json().await?;
        
        let mut prices = HashMap::new();
        if let Some(data_obj) = data["data"].as_object() {
            for (mint, price_data) in data_obj {
                if let Some(price) = price_data["price"].as_f64() {
                    prices.insert(mint.clone(), price);
                }
            }
        }
        
        Ok(prices)
    }

    /// Check if token is valuable enough to sell (worth more than $1)
    fn is_valuable_token(&self, token: &TokenAccount, prices: &HashMap<String, f64>) -> bool {
        let price = prices.get(&token.mint).copied().unwrap_or(0.0);
        let value = token.ui_amount * price;
        value > 1.0 // Only sell tokens worth more than $1
    }

    /// Sell a specific token using Jupiter API
    async fn sell_token(&self, rpc_client: &RpcClient, wallet: &Keypair, token: &TokenAccount) -> Result<String> {
        self.logger.log(format!("💱 Selling {} tokens of mint: {}", token.ui_amount, token.mint));

        // Get quote from Jupiter
        let amount_in_smallest_unit = token.amount;
        
        let quote_url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps=100",
            self.api_url, token.mint, SOL_MINT, amount_in_smallest_unit
        );

        let quote_response = self.client.get(&quote_url).send().await?;
        if !quote_response.status().is_success() {
            return Err(anyhow!("Failed to get quote from Jupiter: {}", quote_response.status()));
        }
        
        let quote: Value = quote_response.json().await?;

        // Get swap transaction
        let swap_data = json!({
            "quoteResponse": quote,
            "userPublicKey": wallet.pubkey().to_string(),
            "wrapAndUnwrapSol": true,
            "dynamicComputeUnitLimit": true,
            "prioritizationFeeLamports": {
                "priorityLevelWithMaxLamports": {
                    "maxLamports": 1000000,
                    "priorityLevel": "high"
                }
            }
        });

        let swap_url = format!("{}/swap", self.api_url);
        let swap_response = self.client.post(&swap_url)
            .json(&swap_data)
            .send()
            .await?;

        if !swap_response.status().is_success() {
            return Err(anyhow!("Failed to get swap transaction from Jupiter: {}", swap_response.status()));
        }

        let swap_result: Value = swap_response.json().await?;
        
        let swap_transaction = swap_result["swapTransaction"]
            .as_str()
            .ok_or_else(|| anyhow!("No swap transaction in response"))?;

        // Decode and sign the transaction
        let transaction_bytes = base64::decode(swap_transaction)?;
        let mut transaction: VersionedTransaction = bincode::deserialize(&transaction_bytes)?;

        // Sign the transaction
        transaction.sign(&[wallet], rpc_client.get_latest_blockhash().await?);

        // Send the transaction with retry logic
        let mut signature = None;
        let max_attempts = 3;
        
        for attempt in 1..=max_attempts {
            match rpc_client.send_and_confirm_transaction(&transaction.into()).await {
                Ok(sig) => {
                    signature = Some(sig);
                    break;
                },
                Err(e) => {
                    self.logger.log(format!("❌ Sell attempt {}/{} failed: {}", attempt, max_attempts, e).red().to_string());
                    if attempt >= max_attempts {
                        return Err(anyhow!("Transaction failed after {} attempts: {}", max_attempts, e));
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
        }

        let signature = signature.ok_or_else(|| anyhow!("Failed to get transaction signature"))?;

        let estimated_sol_received = quote["outAmount"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|amount| amount as f64 / 1e9)
            .unwrap_or(0.0);

        self.logger.log(format!(
            "✅ Token sale completed: {} -> {:.6} SOL (signature: {})",
            token.mint, estimated_sol_received, signature
        ));

        Ok(format!("Successfully sold {:.6} tokens for {:.6} SOL", token.ui_amount, estimated_sol_received))
    }

    /// Sell all tokens in the wallet
    pub async fn sell_all_tokens(
        &self,
        rpc_client: &RpcClient,
        wallet: &Keypair,
        slippage_bps: Option<u32>,
    ) -> Result<HashMap<String, String>> {
        self.logger.log("🔍 Getting all token accounts...".to_string());
        
        let wallet_pubkey = wallet.pubkey();
        let token_accounts = self.get_wallet_token_accounts(rpc_client, &wallet_pubkey).await?;
        
        if token_accounts.is_empty() {
            self.logger.log("ℹ️ No token accounts found to sell".to_string());
            return Ok(HashMap::new());
        }

        self.logger.log(format!("📊 Found {} token accounts", token_accounts.len()));

        // Get token prices for value assessment
        let mints: Vec<String> = token_accounts.iter().map(|t| t.mint.clone()).collect();
        let prices = self.get_token_prices(&mints).await?;

        // Filter valuable tokens
        let valuable_tokens: Vec<&TokenAccount> = token_accounts
            .iter()
            .filter(|token| self.is_valuable_token(token, &prices))
            .collect();

        self.logger.log(format!("💱 Found {} valuable tokens to sell", valuable_tokens.len()));

        let mut results = HashMap::new();

        for token in valuable_tokens {
            match self.sell_token(rpc_client, wallet, token).await {
                Ok(result) => {
                    results.insert(token.mint.clone(), result);
                },
                Err(e) => {
                    let error_msg = format!("Error: {}", e);
                    self.logger.log(format!("❌ Failed to sell {}: {}", token.mint, e).red().to_string());
                    results.insert(token.mint.clone(), error_msg);
                }
            }
        }

        Ok(results)
    }
}

#[derive(Debug, Clone)]
struct TokenAccount {
    mint: String,
    amount: u64,
    decimals: u8,
    ui_amount: f64,
} 
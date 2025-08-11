use crate::jupiter_api::{JupiterClient, Quote, Token};
use crate::solana::SolanaClient;
use crate::config::Config;
use crate::flash_loan::{FlashLoanContext, FlashBorrowReserveLiquidityArgs, FlashRepayReserveLiquidityArgs, create_flash_borrow_instruction, create_flash_repay_instruction, create_or_get_flash_loan_lookup_table};
use crate::jito;
use crate::telegram::TelegramNotifier;
use anyhow::{Result, anyhow};
use log::{info,  debug, warn};
use std::sync::Arc;
use base64::{Engine, prelude::BASE64_STANDARD};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::{Instruction, AccountMeta},
    pubkey::Pubkey,
    address_lookup_table_account::AddressLookupTableAccount,
    message::v0::Message as MessageV0,
    transaction::VersionedTransaction,
    hash::Hash,
    message::VersionedMessage,
    signer::Signer,
    transaction::Transaction
};
use solana_address_lookup_table_program::{self, state::AddressLookupTable};
use serde_json;
use reqwest;
use serde::Deserialize;
use std::str::FromStr;
use std::env;
use jupiter_sdk::generated::instructions::route::{
    Route, 
    RouteInstructionArgs
};
use jupiter_sdk::generated::types::{RoutePlanStep, Swap, Side};
use jupiter_sdk::generated::programs::JUPITER_ID;
use spl_associated_token_account::get_associated_token_address;
use std::collections::{HashMap, HashSet};

pub struct ArbitrageScanner {
    pub jupiter_client: Arc<JupiterClient>,
    pub config: Arc<Config>,
    pub token_a: Token,
    pub token_b: Token,
    pub iteration: u64,
    pub max_profit_spotted: f64,
}

impl ArbitrageScanner {
    pub fn new(
        jupiter_client: Arc<JupiterClient>,
        config: Arc<Config>,
        token_a: Token,
        token_b: Token,
    ) -> Self {            
        Self {
            jupiter_client,
            config,
            token_a,
            token_b,
            iteration: 0,
            max_profit_spotted: 0.0,
        }
    }
    
    pub async fn scan_for_opportunity(&mut self) -> Result<Option<ArbitrageOpportunity>> {
        self.iteration += 1;
        
        // Calculate trade amount in lamports
        let trade_amount = (self.config.trade_size_sol * 1_000_000_000.0) as u64;
        
        // Get quote for A -> B
        let quote_a_to_b: Quote = self.jupiter_client.get_quote(
            &self.token_a.address,
            &self.token_b.address,
            trade_amount,
            self.config.slippage_bps,
        ).await?;
        
        debug!("A->B Quote: {:?}", quote_a_to_b);
        
        // Get quote for B -> A
        let out_amount_a_to_b = quote_a_to_b.out_amount.parse::<u64>()?;
        let quote_b_to_a = self.jupiter_client.get_quote(
            &self.token_b.address,
            &self.token_a.address,
            out_amount_a_to_b,
            self.config.slippage_bps,
        ).await?;
        
        debug!("B->A Quote: {:?}", quote_b_to_a);
        
        // Calculate profit
        let final_amount = quote_b_to_a.out_amount.parse::<u64>()?;
        let profit_lamports = final_amount as i64 - trade_amount as i64;
        // Calculate raw profit percentage
        let profit_percentage = (profit_lamports as f64 / trade_amount as f64) * 100.0;
        
        // Calculate compute unit cost in SOL
        let compute_unit_cost = 0.00015; // Convert to SOL
        
        // Calculate estimated cost as a percentage of trade amount
        let estimated_cost_percentage = (compute_unit_cost / (trade_amount as f64 / 1_000_000_000.0)) * 100.0;
        
        // Calculate fine-tuned profit percentage accounting for compute costs
        let finetuned_profit_percentage = profit_percentage - estimated_cost_percentage;
        
        // Use the finetuned profit for decision making, but keep the raw profit for reporting

        let profit_percentage = finetuned_profit_percentage;
        
        // Update max profit spotted
        if profit_percentage > self.max_profit_spotted {
            self.max_profit_spotted = profit_percentage;
            info!("New max profit spotted: {}% ({} SOL) with {} -> {} -> {}", 
                self.max_profit_spotted,
                profit_lamports as f64 / 1_000_000_000.0,
                self.token_a.symbol,
                self.token_b.symbol,
                self.token_a.symbol);
        }
        
        // Log periodic updates about the current profit
        if self.iteration % 100 == 0 {
            debug!("Current profit: {}%, Max profit: {}%, Tokens: {} -> {} -> {}", 
                profit_percentage, 
                self.max_profit_spotted,
                self.token_a.symbol,
                self.token_b.symbol,
                self.token_a.symbol);
        }
        
        // Check if profitable
        if profit_percentage >= self.config.min_profit_threshold {
            info!("Profitable arbitrage opportunity found: {}% ({} SOL)", 
                profit_percentage, 
                profit_lamports as f64 / 1_000_000_000.0);
            
            return Ok(Some(ArbitrageOpportunity {
                token_a: self.token_a.clone(),
                token_b: self.token_b.clone(),
                quote_a_to_b,
                quote_b_to_a,
                profit_lamports,
                profit_percentage,
                trade_amount: trade_amount.to_string(),
            }));
        }
        
        warn!("No profitable arbitrage opportunity found for {} -> {} -> {} in threshold {} (current profit: {}%)", 
            self.token_a.symbol, self.token_b.symbol, self.token_a.symbol, 
            self.config.min_profit_threshold, profit_percentage);
            
        Ok(None)
    }
}

pub struct ArbitrageExecutor {
    pub solana_client: Arc<SolanaClient>,
    pub config: Arc<Config>,
    telegram_notifier: TelegramNotifier,
    flash_borrow_instruction: Option<Instruction>,
    flash_repay_instruction: Option<Instruction>,
    flash_loan_ctx: Option<FlashLoanContext>,
    flash_loan_lookup_table: Option<String>,
}

impl ArbitrageExecutor {
    pub fn new(
        solana_client: Arc<SolanaClient>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            solana_client,
            config,
            telegram_notifier: TelegramNotifier::new(),
            flash_borrow_instruction: None,
            flash_repay_instruction: None,
            flash_loan_ctx: None,
            flash_loan_lookup_table: None,
        }
    }
    
    // Add a method to initialize flash loan instructions
    pub async fn initialize_flash_loan(&mut self) -> Result<()> {
        if !self.config.use_flash_loan {
            info!("Flash loan is disabled, skipping flash loan initialization");
            return Ok(());
        }
        
        info!("Initializing flash loan instructions at startup...");
        
        // Start timing
        let start_time = std::time::Instant::now();
        
        // Create flash loan context
        let flash_loan_ctx = FlashLoanContext::new(
            Arc::clone(&self.solana_client),
            &env::var("LENDING_MARKET_AUTHORITY").unwrap_or_default(),
            &env::var("LENDING_MARKET_ADDRESS").unwrap_or_default(),
            &env::var("RESERVE_ADDRESS").unwrap_or_default(),
            &env::var("RESERVE_LIQUIDITY_MINT").unwrap_or_default(),
            &env::var("RESERVE_SOURCE_LIQUIDITY").unwrap_or_default(),
            &env::var("RESERVE_LIQUIDITY_FEE_RECEIVER").unwrap_or_default(),
            &env::var("REFERER_TOKEN_STATE").unwrap_or_default(),
            &env::var("REFERER_ACCOUNT").unwrap_or_default(),
        ).await?;
        
        // Fixed flash loan amount (10,000 SOL in lamports)
        let flash_amount: u64 = 10000000000000;
        
        // Create flash borrow and repay args
        let borrow_args = FlashBorrowReserveLiquidityArgs {
            amount: flash_amount,
        };

        let repay_args = FlashRepayReserveLiquidityArgs {
            amount: flash_amount,
            borrow_instruction_index: 0, // This will be adjusted in execute_arbitrage
        };
        
        // Create flash loan instructions
        let borrow_ix = create_flash_borrow_instruction(&flash_loan_ctx, &borrow_args)?;
        let repay_ix = create_flash_repay_instruction(&flash_loan_ctx, &repay_args)?;
        
        // Create or get flash loan lookup table
        let lookup_table_addr = create_or_get_flash_loan_lookup_table(&self.solana_client).await?;
        
        // Save the instructions, context, and lookup table address
        self.flash_borrow_instruction = Some(borrow_ix);
        self.flash_repay_instruction = Some(repay_ix);
        self.flash_loan_ctx = Some(flash_loan_ctx);
        self.flash_loan_lookup_table = Some(lookup_table_addr);
        
        // Record elapsed time
        let elapsed = start_time.elapsed();
        info!("Flash loan instructions created successfully for fixed amount of 10,000 SOL in {:?}", elapsed);
        
        Ok(())
    }
    
    pub async fn execute_arbitrage(&self, opportunity: ArbitrageOpportunity) -> Result<ArbitrageResult> {
        info!("Executing arbitrage opportunity: {} -> {} -> {}", 
            opportunity.token_a.symbol, 
            opportunity.token_b.symbol, 
            opportunity.token_a.symbol);
            
        // Calculate profit in SOL for use in notifications
        let profit_sol = opportunity.profit_lamports as f64 / 1_000_000_000.0;
        
        // Try to execute the arbitrage transaction, with error handling for notifications
        let result = self.execute_transaction(&opportunity).await;
        
        match &result {
            Ok(arb_result) => {
                if arb_result.success {
                    // Transaction successful - send success notification
                    if let Some(signature) = &arb_result.signature {
                        if let Err(e) = self.telegram_notifier.send_transaction_notification(
                            signature,
                            &opportunity.token_a.symbol,
                            &opportunity.token_b.symbol,
                            opportunity.profit_percentage,
                            profit_sol
                        ).await {
                            warn!("Failed to send Telegram success notification: {}", e);
                        }
                    }
                } else if let Some(error) = &arb_result.error {
                    // Transaction failed with known error - send error notification
                    if let Err(e) = self.telegram_notifier.send_error_notification(
                        &format!("Transaction failed: {}\nTrade: {} → {} → {}\nProfit: {:.4}% ({:.6} SOL)",
                            error,
                            opportunity.token_a.symbol,
                            opportunity.token_b.symbol,
                            opportunity.token_a.symbol,
                            opportunity.profit_percentage,
                            profit_sol
                        )
                    ).await {
                        warn!("Failed to send Telegram error notification: {}", e);
                    }
                }
            },
            Err(e) => {
                // Execution error - send error notification
                if let Err(notify_err) = self.telegram_notifier.send_error_notification(
                    &format!("Error executing arbitrage: {}\nTrade: {} → {} → {}\nProfit: {:.4}% ({:.6} SOL)",
                        e,
                        opportunity.token_a.symbol,
                        opportunity.token_b.symbol,
                        opportunity.token_a.symbol,
                        opportunity.profit_percentage,
                        profit_sol
                    )
                ).await {
                    warn!("Failed to send Telegram error notification: {}", notify_err);
                }
            }
        }
        
        // Return the result regardless of notification status
        result
    }
    
    // Extract the transaction execution logic to a separate method
    async fn execute_transaction(&self, opportunity: &ArbitrageOpportunity) -> Result<ArbitrageResult> {
        // Ensure all required token accounts exist
        let token_accounts = self.ensure_token_accounts_for_arbitrage(opportunity).await?;
        
        // When building the swap instructions, use the correct token accounts
        // For example, for the A->B swap:
        let a_token_account = *token_accounts.get(&opportunity.quote_a_to_b.input_mint)
            .ok_or_else(|| anyhow!("Missing input token account"))?;
        let b_token_account = *token_accounts.get(&opportunity.quote_a_to_b.output_mint)
            .ok_or_else(|| anyhow!("Missing output token account"))?;
        
        // Log the token accounts being used
        info!("Using token accounts: A={}, B={}", a_token_account, b_token_account);
        
        // Add initialization checks and account creation
         
        let mut instructions = Vec::new();
        // Add default compute budget instructions
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(523095));
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(187777));

        // Check if we should use flash loan based on config
        let is_use_flash_loan = self.config.use_flash_loan;
        info!("Flash loan enabled: {}", is_use_flash_loan);

        // Get swap instructions for A -> B
        let swap_instructions_a_to_b = get_swap_instructions(
            &opportunity.quote_a_to_b,
            &self.solana_client.wallet_pubkey().to_string()
        ).await?;
        
        // Get swap instructions for B -> A
        let swap_instructions_b_to_a = get_swap_instructions(
            &opportunity.quote_b_to_a,
            &self.solana_client.wallet_pubkey().to_string()
        ).await?;

        // Create flash loan instructions if enabled - USE PRE-CREATED INSTRUCTIONS
        let (flash_borrow, flash_repay) = if is_use_flash_loan {
            // Start timing
            let start_time = std::time::Instant::now();
            
            let result = if let (Some(borrow_ix), Some(repay_ix)) = (&self.flash_borrow_instruction, &self.flash_repay_instruction) {
                info!("Using pre-created flash loan instructions");
                (Some(borrow_ix.clone()), Some(repay_ix.clone()))
            } else {
                info!("Pre-created flash loan instructions not found, creating new ones (this is unexpected)");
                
                // Fallback to creating instructions on the fly
                let ctx = FlashLoanContext::new(
                    Arc::clone(&self.solana_client),
                    &env::var("LENDING_MARKET_AUTHORITY").unwrap_or_default(),
                    &env::var("LENDING_MARKET_ADDRESS").unwrap_or_default(),
                    &env::var("RESERVE_ADDRESS").unwrap_or_default(),
                    &env::var("RESERVE_LIQUIDITY_MINT").unwrap_or_default(),
                    &env::var("RESERVE_SOURCE_LIQUIDITY").unwrap_or_default(),
                    &env::var("RESERVE_LIQUIDITY_FEE_RECEIVER").unwrap_or_default(),
                    &env::var("REFERER_TOKEN_STATE").unwrap_or_default(),
                    &env::var("REFERER_ACCOUNT").unwrap_or_default(),
                ).await?;
                
                // Create flash borrow and repay args
                let borrow_args = FlashBorrowReserveLiquidityArgs {
                    amount: 10000000000000,
                };

                let repay_args = FlashRepayReserveLiquidityArgs {
                    amount: 10000000000000,
                    borrow_instruction_index: 2,
                };
                
                // Create flash loan instructions
                let borrow_ix = create_flash_borrow_instruction(&ctx, &borrow_args)?;
                let repay_ix = create_flash_repay_instruction(&ctx, &repay_args)?;
                
                (Some(borrow_ix), Some(repay_ix))
            };
            
            // Record elapsed time and log performance metrics
            let elapsed = start_time.elapsed();
            info!("Flash loan instructions preparation took {:?}", elapsed);
            
            result
        } else {
            (None, None)
        };

        let mut combined_accounts = Vec::new();
        let accounts_a_to_b: Result<Vec<_>> = swap_instructions_a_to_b.swap_instruction.accounts
            .iter()
            .skip(9) 
            .map(|a| convert_to_account_meta(a))
            .collect();
        
        let accounts_b_to_a: Result<Vec<_>> = swap_instructions_b_to_a.swap_instruction.accounts
            .iter()
            .skip(9) 
            .map(|a| convert_to_account_meta(a))
            .collect();

        let accounts_a_to_b = accounts_a_to_b?;
        let accounts_b_to_a = accounts_b_to_a?;
            
        // Extend combined_accounts with the vectors
        combined_accounts.extend(accounts_a_to_b.clone());
        combined_accounts.extend(accounts_b_to_a.clone());

        let is_use_combined_ix = check_is_use_combined_ix(
            &opportunity.quote_a_to_b,
            &opportunity.quote_b_to_a,
        );
        info!("Using combined instruction: {}", is_use_combined_ix);
        
        if is_use_combined_ix {
            info!("======= Combining swap instructions!!!!! =======");
            // Create the combined swap instruction
            let combined_swap = combine_swap_instructions(
                &opportunity.quote_a_to_b,
                &opportunity.quote_b_to_a,
                opportunity.quote_a_to_b.in_amount.parse::<u64>()?,
                opportunity.quote_b_to_a.out_amount.parse::<u64>()?,
                self.config.slippage_bps as u16,
                &self.solana_client,
                &combined_accounts
            )?;
            if !is_use_flash_loan {
                // Add setup instructions for A -> B
                if let Some(setup_instructions) = &swap_instructions_a_to_b.setup_instructions {
                    for instruction in setup_instructions {
                        instructions.push(deserialize_instruction(instruction)?);
                    }
                }
                
                // Add setup instructions for B -> A
                if let Some(setup_instructions) = &swap_instructions_b_to_a.setup_instructions {
                    for instruction in setup_instructions {
                        instructions.push(deserialize_instruction(instruction)?);
                    }
                }
            }

            // Add flash loan instructions if enabled
            if is_use_flash_loan {
                instructions.insert(0, flash_borrow.unwrap());
                instructions.push(combined_swap.clone());
                instructions.push(flash_repay.unwrap());
            } else {
                instructions.push(combined_swap.clone());
            }
        } else {
            // When it is on flash loan , we don't need to add setup instructions , flash loan will handle it
            if !is_use_flash_loan {
                // Add setup instructions for A -> B
                if let Some(setup_instructions) = &swap_instructions_a_to_b.setup_instructions {
                    for instruction in setup_instructions {
                        instructions.push(deserialize_instruction(instruction)?);
                    }
                }
                
                // Add setup instructions for B -> A
                if let Some(setup_instructions) = &swap_instructions_b_to_a.setup_instructions {
                    for instruction in setup_instructions {
                        instructions.push(deserialize_instruction(instruction)?);
                    }
                }
            }
            // Add flash loan instructions if enabled
            if is_use_flash_loan {
                instructions.insert(0, flash_borrow.unwrap());
                instructions.push(deserialize_instruction(&swap_instructions_a_to_b.swap_instruction)?);
                instructions.push(deserialize_instruction(&swap_instructions_b_to_a.swap_instruction)?);
                instructions.push(flash_repay.unwrap());
            } else {
                instructions.push(deserialize_instruction(&swap_instructions_a_to_b.swap_instruction)?);
                instructions.push(deserialize_instruction(&swap_instructions_b_to_a.swap_instruction)?);
            }
        }
        
        // Get recent blockhash
        let recent_blockhash = self.solana_client.get_latest_blockhash().await?;
        info!("Got recent blockhash: {:?}", recent_blockhash);

        // Load address lookup tables
        let mut lookup_tables = Vec::new();
        for address_str in swap_instructions_a_to_b.address_lookup_table_addresses.unwrap_or_default() {
            let address = Pubkey::from_str(&address_str)?;
            
            // Fetch and deserialize lookup table
            let account = self.solana_client.get_account(&address).await?;
            let lookup_table = AddressLookupTable::deserialize(&account.data)
                .map_err(|_| anyhow!("Failed to deserialize lookup table"))?;
            
            lookup_tables.push(AddressLookupTableAccount {
                key: address,
                addresses: lookup_table.addresses.to_vec(),
            });
        }
        for address_str in swap_instructions_b_to_a.address_lookup_table_addresses.unwrap_or_default() {
            let address = Pubkey::from_str(&address_str)?;
            
            // Fetch and deserialize lookup table
            let account = self.solana_client.get_account(&address).await?;
            let lookup_table = AddressLookupTable::deserialize(&account.data)
                .map_err(|_| anyhow!("Failed to deserialize lookup table"))?;
            
            lookup_tables.push(AddressLookupTableAccount {
                key: address,
                addresses: lookup_table.addresses.to_vec(),
            });
        }
        
        // Add flash loan lookup table if flash loan is used
        if is_use_flash_loan {
            if let Some(lookup_table_addr_str) = &self.flash_loan_lookup_table {
                info!("Adding flash loan address lookup table: {}", lookup_table_addr_str);
                
                let address = Pubkey::from_str(lookup_table_addr_str)?;
                
                // Fetch and deserialize lookup table
                match self.solana_client.get_account(&address).await {
                    Ok(account) => {
                        match AddressLookupTable::deserialize(&account.data) {
                            Ok(lookup_table) => {
                                lookup_tables.push(AddressLookupTableAccount {
                                    key: address,
                                    addresses: lookup_table.addresses.to_vec(),
                                });
                                info!("Successfully added flash loan lookup table with {} addresses", 
                                      lookup_table.addresses.len());
                            },
                            Err(e) => {
                                warn!("Failed to deserialize flash loan lookup table: {}", e);
                            }
                        }
                    },
                    Err(e) => {
                        warn!("Failed to fetch flash loan lookup table account: {}", e);
                    }
                }
            } else {
                warn!("Flash loan is enabled but no lookup table address is available");
            }
        }

        
        // Create versioned transaction with lookup tables
        let tx = create_versioned_transaction(
            &self.solana_client,
            instructions,
            lookup_tables,
            recent_blockhash,
        )?;

        // Check if Jito bundles are enabled
        let is_use_jito = self.config.use_jito_bundle;
        
        // log instructions lenth
        info!("Instructions length: {}", tx.message.instructions().len());
        
        // Send the transaction
        let signature = if is_use_jito {
            info!("Sending transaction via Jito bundle with tip...");
            jito::send_bundle_with_jito_tip(
                &self.solana_client,
                &tx,
                recent_blockhash
            ).await?
        } else {
            info!("Sending transaction via standard RPC...");
            self.solana_client.send_versioned_transaction(&tx).await?
        };
        
        info!("Transaction sent successfully with signature: {}", signature);

        Ok(ArbitrageResult {
            signature: Some(signature),
            error: None,
            profit_percentage: opportunity.profit_percentage,
            success: true,
            output_amount: opportunity.trade_amount.parse::<u64>()?,
            input_amount: opportunity.trade_amount.parse::<u64>()?,
            simulated: false,
        })
    }

    // Add this function to properly check if a token account exists before creating it
    pub async fn ensure_token_accounts_for_arbitrage(
        &self, 
        opportunity: &ArbitrageOpportunity
    ) -> Result<HashMap<String, Pubkey>> {
        let wallet_pubkey = self.solana_client.wallet_pubkey();
        let mut token_accounts = HashMap::new();
        
        // Get all unique token mints involved in the arbitrage
        let mut unique_mints = HashSet::new();
        unique_mints.insert(opportunity.quote_a_to_b.input_mint.clone());
        unique_mints.insert(opportunity.quote_a_to_b.output_mint.clone());
        
        // If there's a multi-step route, add intermediate tokens
        for step in &opportunity.quote_a_to_b.route_plan {
            unique_mints.insert(step.swap_info.input_mint.clone());
            unique_mints.insert(step.swap_info.output_mint.clone());
        }
        
        for step in &opportunity.quote_b_to_a.route_plan {
            unique_mints.insert(step.swap_info.input_mint.clone());
            unique_mints.insert(step.swap_info.output_mint.clone());
        }
        
        // For each unique mint, ensure we have a token account
        for mint in unique_mints {
            let mint_pubkey = Pubkey::from_str(&mint)?;
            let token_account = get_associated_token_address(&wallet_pubkey, &mint_pubkey);
            
            // Check if account exists, create if needed
            let account_exists = self.solana_client.get_account(&token_account).await.is_ok();
            
            if !account_exists {
                info!("Creating token account for mint: {}", mint);
                let create_ata_ix = spl_associated_token_account::instruction::create_associated_token_account(
                    &wallet_pubkey,
                    &wallet_pubkey,
                    &mint_pubkey,
                    &spl_token::id(),
                );
                
                let recent_blockhash = self.solana_client.get_latest_blockhash().await?;
                let transaction = Transaction::new_signed_with_payer(
                    &[create_ata_ix],
                    Some(&wallet_pubkey),
                    &[self.solana_client.get_keypair()],
                    recent_blockhash,
                );
                
                self.solana_client.send_transaction(&transaction).await?;
                info!("Created token account: {}", token_account);
            } else {
                info!("Token account for mint {} already exists: {}", mint, token_account);
            }
            
            token_accounts.insert(mint, token_account);
        }
        
        Ok(token_accounts)
    }
}

// Helper function to get swap instructions from Jupiter API
async fn get_swap_instructions(
    quote: &Quote,
    user_public_key: &str
) -> Result<SwapInstructions> {
    let url = "https://api.jup.ag/swap/v1/swap-instructions";
    
    // Get Jupiter API key from environment variable
    let api_key = std::env::var("JUPITER_API_KEY").unwrap_or_else(|_| {
        warn!("JUPITER_API_KEY not found in environment, using default");
        "b5e5d25a-7897-".to_string()
    });
    
    // Create the request body
    let body = serde_json::json!({
        "quoteResponse": {
            "inputMint": quote.input_mint,
            "inAmount": quote.in_amount,
            "outputMint": quote.output_mint,
            "outAmount": quote.out_amount,
            "otherAmountThreshold": quote.other_amount_threshold,
            "swapMode": quote.swap_mode,
            "slippageBps": quote.slippage_bps,
            "priceImpactPct": quote.price_impact_pct,
            "routePlan": quote.route_plan.iter().map(|rp| {
                serde_json::json!({
                    "swapInfo": {
                        "ammKey": rp.swap_info.amm_key,
                        "label": rp.swap_info.label,
                        "inputMint": rp.swap_info.input_mint,
                        "outputMint": rp.swap_info.output_mint,
                        "inAmount": rp.swap_info.in_amount,
                        "outAmount": rp.swap_info.out_amount,
                        "feeAmount": rp.swap_info.fee_amount,
                        "feeMint": rp.swap_info.fee_mint
                    },
                    "percent": rp.percent
                })
            }).collect::<Vec<_>>(),
            "contextSlot": quote.context_slot
        },
        "userPublicKey": user_public_key,
        "wrapAndUnwrapSol": true,
        "useSharedAccounts": false,
        "dynamicComputeUnitLimit": true
    });
    
    // Send the request
    let client = reqwest::Client::new();
    let response = client.post(url)
        .header("x-api-key", &api_key)
        .json(&body)
        .send()
        .await?;
    
    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Failed to get swap instructions: {}", error_text));
    }
    
    // Parse the response
    let instructions: SwapInstructions = response.json().await?;
    
    Ok(instructions)
}

// Add these new structs for the swap instructions API
#[derive(Debug, Deserialize)]
struct SwapInstructions {
    // #[serde(rename = "computeBudgetInstructions")]
    // pub compute_budget_instructions: Option<Vec<InstructionData>>,
    
    #[serde(rename = "setupInstructions")]
    pub setup_instructions: Option<Vec<InstructionData>>,
    
    #[serde(rename = "swapInstruction")]
    pub swap_instruction: InstructionData,
    
    // #[serde(rename = "cleanupInstruction")]
    // pub cleanup_instruction: Option<InstructionData>,
    
    #[serde(rename = "addressLookupTableAddresses")]
    pub address_lookup_table_addresses: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct InstructionData {
    #[serde(rename = "programId")]
    pub program_id: String,
    
    pub accounts: Vec<AccountData>,
    
    pub data: String,
}

#[derive(Debug, Deserialize, Clone)]
struct AccountData {
    pub pubkey: String,
    #[serde(rename = "isSigner")]
    pub is_signer: bool,
    #[serde(rename = "isWritable")]
    pub is_writable: bool,
}

#[derive(Debug)]
pub struct ArbitrageOpportunity {
    pub token_a: Token,
    pub token_b: Token,
    pub quote_a_to_b: Quote,
    pub quote_b_to_a: Quote,
    pub profit_lamports: i64,
    pub profit_percentage: f64,
    pub trade_amount: String,
}

pub struct ArbitrageResult {
    pub signature: Option<String>,
    pub error: Option<String>,
    pub profit_percentage: f64,
    pub success: bool,
    pub output_amount: u64,
    pub input_amount: u64,
    pub simulated: bool,
}

fn convert_to_account_meta(account: &AccountData) -> Result<AccountMeta> {
    Ok(AccountMeta {
        pubkey: Pubkey::from_str(&account.pubkey)?,
        is_signer: account.is_signer,
        is_writable: account.is_writable,
    })
}

// Add this function to combine the swap instructions
pub fn combine_swap_instructions(
    quote_a_to_b: &Quote,
    quote_b_to_a: &Quote,
    trade_input_amount: u64,
    trade_output_amount: u64,
    slippage_bps: u16,
    solana_client: &SolanaClient,
    remaining_accounts: &[AccountMeta],
) -> Result<Instruction> {
    // Create the combined route plan
    let mut route_plan = Vec::new();
    
    info!("Creating combined route plan from {} A->B steps and {} B->A steps", 
          quote_a_to_b.route_plan.len(), 
          quote_b_to_a.route_plan.len());
    
    // Add all steps from A->B route
    for (i, step) in quote_a_to_b.route_plan.iter().enumerate() {
        // let input_mint: u64 = step.swap_info.input_mint.parse()?;
        // let output_mint: u64 = step.swap_info.output_mint.parse()?;
        // let swap = map_amm_to_swap(&step.swap_info.label, true, input_mint, output_mint);
        let swap = map_amm_to_swap(&step.swap_info.label, true);
        info!("A->B step {}: {} (input_index: {}, output_index: {})", 
        i, step.swap_info.label, i, i+1);
        
        route_plan.push(RoutePlanStep {
            swap,
            percent: 100,
            input_index: i as u8,
            output_index: (i + 1) as u8,
        });
    }
    
    // Add all steps from B->A route
    let a_to_b_len = route_plan.len();
    for (i, step) in quote_b_to_a.route_plan.iter().enumerate() {
        let output_index = if i == quote_b_to_a.route_plan.len() - 1 {
            0 // Last step should output to index 0 (completing the circle)
        } else {
            (a_to_b_len + i + 1) as u8
        };
        
        // let input_mint1: u64 = step.swap_info.input_mint.parse()?;
        // let output_mint2: u64 = step.swap_info.output_mint.parse()?;
        // let swap = map_amm_to_swap(&step.swap_info.label, false, input_mint1, output_mint2);
        let swap = map_amm_to_swap(&step.swap_info.label, false);
        info!("B->A step {}: {} (input_index: {}, output_index: {})", 
              i, step.swap_info.label, a_to_b_len + i, output_index);
        
        route_plan.push(RoutePlanStep {
            swap,
            percent: 100,
            input_index: (a_to_b_len + i) as u8,
            output_index,
        });
    }

    let user_authority = solana_client.wallet_pubkey();
    let source_mint = Pubkey::from_str(&quote_a_to_b.input_mint)?;
    let destination_mint = Pubkey::from_str(&quote_b_to_a.output_mint)?;
    
    // Get associated token accounts
    let source_account = get_associated_token_address(
        &user_authority,
        &source_mint,
    );
    let dest_account = get_associated_token_address(
        &user_authority,
        &destination_mint,
    );

    //jupiter aggregator event authority
    let event_authority = Pubkey::from_str("D8cy77BBepLMngZx6ZukaTff5hCt1HrWyKk3Hnd9oitf")?; 

    println!("source_account: {:?} , dest_account: {:?}", source_account, dest_account);
    
    // Create the shared accounts route instruction
    let accounts = Route {
        token_program: spl_token::id(),
        user_transfer_authority: user_authority,
        user_source_token_account: source_account,
        user_destination_token_account: dest_account,
        destination_token_account: Some(JUPITER_ID),
        destination_mint,
        platform_fee_account: Some(JUPITER_ID),
        event_authority,
        program: JUPITER_ID,
    };

    // Clone route_plan to avoid moving the original value
    let args = RouteInstructionArgs {
        route_plan: route_plan.clone(),
        in_amount: trade_input_amount,
        quoted_out_amount: trade_output_amount,
        slippage_bps,
        platform_fee_bps: 0,
    };

    // Log the final route plan
    info!("Final route plan has {} steps", args.route_plan.len());
    for (i, step) in args.route_plan.iter().enumerate() {
        info!("Step {}: input_index: {}, output_index: {}", 
              i, step.input_index, step.output_index);
    }
    
    // Create the instruction
    let mut instruction = accounts.instruction_with_remaining_accounts(args, remaining_accounts);
    
    // Fix the signer writability issue - ensure the user authority is marked as writable
    for account in instruction.accounts.iter_mut() {
        if account.pubkey == user_authority && account.is_signer {
            account.is_writable = true;
            info!("Set user authority account {} as writable", account.pubkey);
        }
    }
    info!("Created combined swap instruction with {} bytes of data", instruction.data.len());
    
    // Log the accounts after modification
    info!("Instruction accounts after modification:");
    for (i, account) in instruction.accounts.iter().enumerate() {
        if account.is_signer {
            info!("  Account #{}: {} (signer: {}, writable: {})", 
                i, account.pubkey, account.is_signer, account.is_writable);
        }
    }
    
    Ok(instruction)
}
 
fn create_versioned_transaction(
    solana_client: &SolanaClient,
    instructions: Vec<Instruction>,
    address_lookup_tables: Vec<AddressLookupTableAccount>,
    recent_blockhash: Hash,
) -> Result<VersionedTransaction> {
    info!("Creating versioned transaction with {} instructions", instructions.len());
    
    // Create v0 message
    let message = MessageV0::try_compile(
        &solana_client.wallet_pubkey(),
        &instructions,
        &address_lookup_tables,
        recent_blockhash,
    )?;

    // Get the keypair for signing
    let signer = solana_client.get_keypair();
    info!("Using signer pubkey: {}", signer.pubkey());
    
    // Create and sign versioned transaction
    let tx = VersionedTransaction::try_new(
        VersionedMessage::V0(message),
        &[signer],
    )?;
    
    info!("Created and signed versioned transaction");
    info!("- Number of signatures: {}", tx.signatures.len());
    
    Ok(tx)
}

// Update the map_amm_to_swap function to use only valid Swap enum variants
fn map_amm_to_swap(amm_name: &str, a_to_b: bool) -> Swap {
    match amm_name {
        "Saber" => Swap::Saber,
        "Raydium CLMM" => Swap::RaydiumClmm,
        "Raydium CLMM V2" => Swap::RaydiumClmmV2,
        "Raydium" => Swap::Raydium,
        "Raydium CP" => Swap::RaydiumCP,
        "Aldrin" => Swap::Aldrin {
            side: Side::Bid
        },
        "Aldrin V2" => Swap::AldrinV2 {
            side: Side::Bid
        },
        "Whirlpool" => Swap::Whirlpool {
            a_to_b,
        },
        "Orca V1" => Swap::TokenSwap,
        "Orca V2" => Swap::WhirlpoolSwapV2 {
            a_to_b,
            remaining_accounts_info: None
        },
        "Orca Aquafarm" => Swap::TokenSwap,
        "Crema" => Swap::Crema {
            a_to_b
        },
        "Cropper" => Swap::Cropper,
        "Lifinity V2" => Swap::LifinityV2,
        "Mercurial" => Swap::Mercurial,
        "Cykura" => Swap::Cykura,
        "Serum" => Swap::Serum {
            side: Side::Bid
        },
        "Step" => Swap::Step,
        "Sencha" => Swap::Sencha,
        "Invariant" => Swap::Invariant {
            x_to_y: a_to_b
        },
        "Meteora DLMM" => Swap::MeteoraDlmm,
        "Meteora" => Swap::Meteora,
        "GooseFX" => Swap::GooseFX,
        "DeltaFi" => Swap::DeltaFi {
            stable: false
        },
        "ZeroFi" => Swap::ZeroFi,
        "Balansol" => Swap::Balansol,
        "Dradex" => Swap::Dradex {
            side: Side::Bid
        },
        "Openbook" => Swap::Openbook {
            side: Side::Bid
        },
        "Phoenix" => Swap::Phoenix {
            side: Side::Bid
        },
        "Symmetry" => Swap::Symmetry {
            from_token_id: 0,
            to_token_id: 0
        },
        "Saber Add Decimals" => if a_to_b {
            Swap::SaberAddDecimalsDeposit
        } else {
            Swap::SaberAddDecimalsWithdraw
        },
        "Marinade Deposit" => Swap::MarinadeDeposit,
        "Marinade Unstake" => Swap::MarinadeUnstake,
        "Perps" => Swap::Perps,
        "Perps Add Liquidity" => Swap::PerpsAddLiquidity,
        "Perps Remove Liquidity" => Swap::PerpsRemoveLiquidity,
        "Perps V2 Add Liquidity" => Swap::PerpsV2AddLiquidity,
        "Perps V2 Remove Liquidity" => Swap::PerpsV2RemoveLiquidity,
        "TokenSwap" => Swap::TokenSwap,
        "Oasis" => Swap::Oasis {
            x_to_y: a_to_b
        },
        "SolFi" => Swap::SolFi {
            is_quote_to_base: a_to_b
        },
        "Perena" => Swap::Perena {
            side: Side::Bid,
            x_to_y: a_to_b
        },
        "DexLab" => Swap::DexLab,
        "Stabble Stable Swap" => Swap::StabbleStableSwap,
        "Stabble Weighted Swap" => Swap::StabbleWeightedSwap,
        "Obric V2" => Swap::Obric {
            x_to_y: a_to_b
        },
        "1DEX" => Swap::OneDex,
        "Pump.fun Amm" => Swap::PumpdotfunAmm,
        "PumpdotfunWrappedBuy" => Swap::PumpdotfunWrappedBuy,
        "PumpdotfunWrappedSell" => Swap::PumpdotfunWrappedSell,
        "MoonshotWrappedBuy" => Swap::MoonshotWrappedBuy,
        "MoonshotWrappedSell" => Swap::MoonshotWrappedSell,
        "GooseFXV2" => Swap::GooseFXV2,
        "OpenBook V2" => Swap::OpenBookV2 {
            side: Side::Bid
        },
        "HeliumTreasuryManagementRedeemV0" => Swap::HeliumTreasuryManagementRedeemV0,
        "StakeDexStakeWrappedSol" => Swap::StakeDexStakeWrappedSol,
        "StakeDexSwapViaStake" => Swap::StakeDexSwapViaStake {
            bridge_stake_seed: 0
        },
        "StakeDexPrefundWithdrawStakeAndDepositStake" => Swap::StakeDexPrefundWithdrawStakeAndDepositStake {
            bridge_stake_seed: 0
        },
        "Clone" => Swap::Clone {
            pool_index: 0,
            quantity_is_input: true,
            quantity_is_collateral: false
        },
        "SanctumS" => Swap::SanctumS {
            src_lst_value_calc_accs: 0,
            dst_lst_value_calc_accs: 0,
            src_lst_index: 0,
            dst_lst_index: 0
        },
        "Sanctum" => Swap::SanctumS {
            src_lst_value_calc_accs: 0,
            dst_lst_value_calc_accs: 0,
            src_lst_index: 0,
            dst_lst_index: 0
        },
        "SanctumSAddLiquidity" => Swap::SanctumSAddLiquidity {
            lst_value_calc_accs: 0,
            lst_index: 0
        },
        "SanctumSRemoveLiquidity" => Swap::SanctumSRemoveLiquidity {
            lst_value_calc_accs: 0,
            lst_index: 0
        },
        "FoxBuyFromEstimatedCost" => Swap::FoxBuyFromEstimatedCost,
        "FoxClaimPartial" => Swap::FoxClaimPartial {
            is_y: false
        },
        "MeteoraDlmm" => Swap::MeteoraDlmm,
        "GooseFX GAMMA" => Swap::GooseFxGamma,
        _ => {
            warn!("Unknown AMM type: {}, defaulting to Whirlpool", amm_name);
            Swap::WhirlpoolSwapV2 {
                a_to_b,
                remaining_accounts_info: None
            }
        }
    }
}

// Helper function to deserialize an instruction
fn deserialize_instruction(instruction: &InstructionData) -> Result<Instruction> {
    let program_id = Pubkey::from_str(&instruction.program_id)?;
    
    let accounts = instruction.accounts.iter().map(|account| {
        let pubkey = Pubkey::from_str(&account.pubkey).unwrap_or_default();
        AccountMeta {
            pubkey,
            is_signer: account.is_signer,
            is_writable: account.is_writable,
        }
    }).collect();
    
    let data = BASE64_STANDARD.decode(&instruction.data)?;
    
    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

fn check_is_use_combined_ix(
    quote_a_to_b: &Quote,
    quote_b_to_a: &Quote,
) -> bool {
    let checklist = Vec::from(["Stabble Weighted Swap", "Raydium", "Meteora DLMM", "Lifinity V2", "whirlpool"]);
    // let checklist = Vec::from(["Stabble Weighted Swap", "Raydium", "Meteora DLMM", "Lifinity V2", "whirlpool", "Lifinity", "Saber", "Perps", "Meteora" , "Cropper", "1DEX", "ZeroFi" , "Stabble Stable Swap", ]);
    let mut is_use_combined_ix = false;
    for route_plan in quote_a_to_b.route_plan.clone() {
        if checklist.contains(&route_plan.swap_info.label.as_str()) {
            is_use_combined_ix = true;
        }
        
        }
    for route_plan in quote_b_to_a.route_plan.clone() {
        if checklist.contains(&route_plan.swap_info.label.as_str()) {
            is_use_combined_ix = true;
        }        
        }

    is_use_combined_ix
}
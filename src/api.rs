use crate::models::*;
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::str::FromStr;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex;
use log::{warn, error};
use std::sync::Arc;

// Official SDK imports for proper order signing
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, OrderType, OrderStatusType, SignatureType};
use polymarket_client_sdk::POLYGON;
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use alloy::primitives::Address as AlloyAddress;
use alloy::primitives::{Address, B256, U256, Bytes};
use alloy::primitives::keccak256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::sol;
use alloy_sol_types::SolCall;

sol! {
    interface IConditionalTokens {
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] indexSets
        ) external;
    }
}



type HmacSha256 = Hmac<Sha256>;

/// Polymarket Gamma API returns token IDs as decimal strings; CLOB SDK expects U256.
fn parse_token_id_to_u256(s: &str) -> Result<U256> {
    let s = s.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        U256::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
            .context("Failed to parse token_id as hex")
    } else {
        U256::from_str_radix(s, 10)
            .context("Failed to parse token_id as decimal")
    }
}

pub struct PolymarketApi {
    client: Client,
    gamma_url: String,
    clob_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    private_key: Option<String>,
    proxy_wallet_address: Option<String>,
    signature_type: Option<u8>,
    authenticated: Arc<tokio::sync::Mutex<bool>>,
}

impl PolymarketApi {
    pub fn new(
        gamma_url: String,
        clob_url: String,
        api_key: Option<String>,
        api_secret: Option<String>,
        api_passphrase: Option<String>,
        private_key: Option<String>,
        proxy_wallet_address: Option<String>,
        signature_type: Option<u8>,
    ) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        
        Self {
            client,
            gamma_url,
            clob_url,
            api_key,
            api_secret,
            api_passphrase,
            private_key,
            proxy_wallet_address,
            signature_type,
            authenticated: Arc::new(tokio::sync::Mutex::new(false)),
        }
    }
    
    // Authenticate with Polymarket CLOB API
    pub async fn authenticate(&self) -> Result<()> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for authentication. Please set private_key in config.json"))?;
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => {
                    warn!("Proxy_wallet_address is set but signature_type is EOA. Defaulting to Proxy.");
                    SignatureType::Proxy
                },
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
            eprintln!("Using proxy wallet: {} (signature type: {:?})", proxy_addr, sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        let _client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials (api_key, api_secret, api_passphrase) and private_key.")?;
        
        *self.authenticated.lock().await = true;
        
        eprintln!("   ✓ Successfully authenticated with Polymarket CLOB API");
        eprintln!("   ✓ Private key: Valid");
        eprintln!("   ✓ API credentials: Valid");
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            eprintln!("   ✓ Proxy wallet: {}", proxy_addr);
        } else {
            eprintln!("   ✓ Trading account: EOA (private key account)");
        }
        Ok(())
    }

    /// Generate HMAC-SHA256 signature for authenticated requests
    fn generate_signature(
        &self,
        method: &str,
        path: &str,
        body: &str,
        timestamp: u64,
    ) -> Result<String> {
        let secret = self.api_secret.as_ref()
            .ok_or_else(|| anyhow::anyhow!("API secret is required for authenticated requests"))?;
        
        let message = format!("{}{}{}{}", method, path, body, timestamp);
        
        let secret_bytes = match base64::decode(secret) {
            Ok(bytes) => bytes,
            Err(_) => {
                secret.as_bytes().to_vec()
            }
        };
        
        // Create HMAC-SHA256 signature
        let mut mac = HmacSha256::new_from_slice(&secret_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to create HMAC: {}", e))?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        let signature = hex::encode(result.into_bytes());
        
        Ok(signature)
    }

    /// Add authentication headers to a request
    fn add_auth_headers(
        &self,
        request: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<reqwest::RequestBuilder> {
        if self.api_key.is_none() || self.api_secret.is_none() || self.api_passphrase.is_none() {
            return Ok(request);
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let signature = self.generate_signature(method, path, body, timestamp)?;
        
        let request = request
            .header("POLY_API_KEY", self.api_key.as_ref().unwrap())
            .header("POLY_SIGNATURE", signature)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_PASSPHRASE", self.api_passphrase.as_ref().unwrap());
        
        Ok(request)
    }

    // Get market by slug (e.g., "btc-updown-15m-1767726000")
    pub async fn get_market_by_slug(&self, slug: &str) -> Result<Market> {
        let url = format!("{}/events/slug/{}", self.gamma_url, slug);
        
        let response = self.client.get(&url).send().await
            .context(format!("Failed to fetch market by slug: {}", slug))?;
        
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market by slug: {} (status: {})", slug, status);
        }
        
        let json: Value = response.json().await
            .context("Failed to parse market response")?;
        
        if let Some(markets) = json.get("markets").and_then(|m| m.as_array()) {
            if let Some(market_json) = markets.first() {
                if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                    return Ok(market);
                }
            }
        }
        
        anyhow::bail!("Invalid market response format: no markets array found")
    }

    // Get order book for a specific token
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book", self.clob_url);
        let params = [("token_id", token_id)];

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch orderbook")?;

        let orderbook: OrderBook = response
            .json()
            .await
            .context("Failed to parse orderbook")?;

        Ok(orderbook)
    }

    /// Get market details by condition ID
    pub async fn get_market(&self, condition_id: &str) -> Result<MarketDetails> {
        let url = format!("{}/markets/{}", self.clob_url, condition_id);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context(format!("Failed to fetch market for condition_id: {}", condition_id))?;

        let status = response.status();
        
        if !status.is_success() {
            anyhow::bail!("Failed to fetch market (status: {})", status);
        }

        let json_text = response.text().await
            .context("Failed to read response body")?;

        let market: MarketDetails = serde_json::from_str(&json_text)
            .map_err(|e| {
                log::error!("Failed to parse market response: {}. Response was: {}", e, json_text);
                anyhow::anyhow!("Failed to parse market response: {}", e)
            })?;

        Ok(market)
    }

    // Get price for a token (for trading)
    pub async fn get_price(&self, token_id: &str, side: &str) -> Result<rust_decimal::Decimal> {
        let url = format!("{}/price", self.clob_url);
        let params = [
            ("side", side),
            ("token_id", token_id),
        ];

        log::debug!("Fetching price from: {}?side={}&token_id={}", url, side, token_id);

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch price")?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("Failed to fetch price (status: {})", status);
        }

        let json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse price response")?;

        let price_str = json.get("price")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid price response format"))?;

        let price = rust_decimal::Decimal::from_str(price_str)
            .context(format!("Failed to parse price: {}", price_str))?;

        log::debug!("Price for token {} (side={}): {}", token_id, side, price);

        Ok(price)
    }

    // Get best bid/ask prices for a token (from orderbook)
    pub async fn get_best_price(&self, token_id: &str) -> Result<Option<TokenPrice>> {
        let orderbook = self.get_orderbook(token_id).await?;
        
        let best_bid = orderbook.bids.first().map(|b| b.price);
        let best_ask = orderbook.asks.first().map(|a| a.price);

        if best_ask.is_some() {
            Ok(Some(TokenPrice {
                token_id: token_id.to_string(),
                bid: best_bid,
                ask: best_ask,
            }))
        } else {
            Ok(None)
        }
    }

    // Place an order
    pub async fn place_order(&self, order: &OrderRequest) -> Result<OrderResponse> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy, // Default to Proxy when proxy wallet is set
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        // Create CLOB client with authentication
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        let side = match order.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", order.side),
        };
        
        let price = rust_decimal::Decimal::from_str(&order.price)
            .context(format!("Failed to parse price: {}", order.price))?;
        let size = rust_decimal::Decimal::from_str(&order.size)
            .context(format!("Failed to parse size: {}", order.size))?;
        
        eprintln!("📤 Creating and posting order: {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);

        let token_id_u256 = parse_token_id_to_u256(&order.token_id)
            .context(format!("Failed to parse token_id as U256: {}", order.token_id))?;

        let order_builder = client
            .limit_order()
            .token_id(token_id_u256)
            .size(size)
            .price(price)
            .side(side);
        
        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign order")?;
        
        // Post order and capture detailed error information
        let response = match client.post_order(signed_order).await {
            Ok(resp) => resp,
            Err(e) => {
                // Log the full error details for debugging
                error!("❌ Failed to post order. Error details: {:?}", e);
                anyhow::bail!(
                    "Failed to post order: {}\n\
                    \n\
                    Troubleshooting:\n\
                    1. Check if you have sufficient USDC balance\n\
                    2. Verify the token_id is valid and active\n\
                    3. Check if the price is within valid range\n\
                    4. Ensure your API credentials have trading permissions\n\
                    5. Verify the order size meets minimum requirements",
                    e
                );
            }
        };
        
        // Check if the response indicates failure even if the request succeeded
        if !response.success {
            let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
            error!("❌ Order rejected by API: {}", error_msg);
            anyhow::bail!(
                "Order was rejected: {}\n\
                \n\
                Order details:\n\
                - Token ID: {}\n\
                - Side: {}\n\
                - Size: {}\n\
                - Price: {}\n\
                \n\
                Common issues:\n\
                1. Insufficient balance or allowance\n\
                2. Invalid token ID or market closed\n\
                3. Price out of range\n\
                4. Size below minimum or above maximum",
                error_msg, order.token_id, order.side, order.size, order.price
            );
        }
        
        // Convert SDK response to our OrderResponse format
        let order_response = OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: Some(format!("Order placed successfully. Order ID: {}", response.order_id)),
        };
        
        eprintln!("✅ Order placed successfully! Order ID: {}", response.order_id);
        
        Ok(order_response)
    }

    // Place a market order (FOK/FAK) for immediate execution
    pub async fn place_market_order(
        &self,
        token_id: &str,
        amount: f64,
        side: &str,
        order_type: Option<&str>, // "FOK" or "FAK", defaults to FOK
    ) -> Result<OrderResponse> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy, // Default to Proxy when proxy wallet is set
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        let side_enum = match side {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", side),
        };
        
        let order_type_enum = match order_type.unwrap_or("FOK") {
            "FOK" => OrderType::FOK,
            "FAK" => OrderType::FAK,
            _ => OrderType::FOK, // Default to FOK
        };
        
        use rust_decimal::{Decimal, RoundingStrategy};
        use rust_decimal::prelude::*;
        
        let amount_decimal = Decimal::from_f64_retain(amount)
            .ok_or_else(|| anyhow::anyhow!("Failed to convert amount to Decimal"))?
            .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
        
        eprintln!("📤 Creating and posting MARKET order: {} {} {} (type: {:?})", 
              side, amount_decimal, token_id, order_type_enum);
        
        let market_price = if matches!(side_enum, Side::Buy) {
            self.get_price(token_id, "SELL")
                .await
                .context("Failed to fetch ASK price for BUY order")?
        } else {
            // For SELL orders, get the BID price (what buyers are bidding - lower price)
            self.get_price(token_id, "BUY")
                .await
                .context("Failed to fetch BID price for SELL order")?
        };
        
        eprintln!("   Using current market price: ${:.4} for {} order", market_price, side);

        let token_id_u256 = parse_token_id_to_u256(token_id)
            .context(format!("Failed to parse token_id as U256: {}", token_id))?;

        let order_builder = client
            .limit_order()
            .token_id(token_id_u256)
            .size(amount_decimal)
            .price(market_price)
            .side(side_enum);
        
        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign market order")?;
        
        let final_price = if matches!(side_enum, Side::Sell) {
            let price_f64 = f64::try_from(market_price).unwrap_or(0.0);
            let adjusted_f64 = price_f64 * 0.995;
            let rounded_f64 = (adjusted_f64 * 100.0).round() / 100.0;
            let final_f64 = rounded_f64.max(0.01);
            Decimal::from_f64_retain(final_f64)
                .ok_or_else(|| anyhow::anyhow!("Failed to convert adjusted price to Decimal"))?
                .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
        } else {
            // For BUY orders, also ensure 2 decimal places
            market_price.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
        };
        
        // If price was adjusted, rebuild the order
        let signed_order = if matches!(side_enum, Side::Sell) && final_price != market_price {
            let final_price_f64 = f64::try_from(final_price).unwrap_or(0.0);
            let market_price_f64 = f64::try_from(market_price).unwrap_or(0.0);
            eprintln!("   ⚠️  Adjusting SELL price from ${:.4} to ${:.4} for immediate execution", market_price_f64, final_price_f64);
            let adjusted_builder = client
                .limit_order()
                .token_id(token_id_u256)
                .size(amount_decimal)
                .price(final_price)
                .side(side_enum);
            client.sign(&signer, adjusted_builder.build().await?)
                .await
                .context("Failed to sign adjusted market order")?
        } else {
            signed_order
        };
        
        // Log detailed order info before posting
        let final_price_f64 = f64::try_from(final_price).unwrap_or(0.0);
        eprintln!("   📋 Order details: Side={}, Size={}, Price=${:.4}, Token={}", 
              side, amount_decimal, final_price_f64, token_id);
        
        let response = match client.post_order(signed_order).await {
            Ok(resp) => resp,
            Err(e) => {
                // Log the full error for debugging
                error!("❌ SDK post_order error: {:?}", e);
                anyhow::bail!(
                    "Failed to post market order: {:?}\n\
                    \n\
                    Order details:\n\
                    - Side: {}\n\
                    - Token ID: {}\n\
                    - Size: {}\n\
                    - Price: ${:.4}\n\
                    \n\
                    Troubleshooting:\n\
                    1. For SELL orders: Verify you own sufficient tokens (check token balance)\n\
                    2. For BUY orders: Verify you have sufficient USDC balance\n\
                    3. Check if token_id is valid and market is active\n\
                    4. Verify price is within valid range (not too low/high)\n\
                    5. Check if order size meets minimum requirements",
                    e, side, token_id, amount_decimal, final_price_f64
                );
            }
        };
        
        // Convert SDK response to our OrderResponse format
        let order_response = OrderResponse {
            order_id: Some(response.order_id.clone()),
            status: response.status.to_string(),
            message: if response.success {
                Some(format!("Market order executed successfully. Order ID: {}", response.order_id))
            } else {
                response.error_msg.clone()
            },
        };
        
        if response.success {
            eprintln!("✅ Market order executed successfully! Order ID: {}", response.order_id);
            Ok(order_response)
        } else {
            let error_msg = response.error_msg.as_deref().unwrap_or("Unknown error");
            anyhow::bail!(
                "Market order failed: {}\n\
                Order ID: {}\n\
                Token ID: {}\n\
                Side: {}\n\
                Size: {}\n\
                Price: ${:.4}\n\
                \n\
                Possible reasons:\n\
                1. Insufficient balance or allowance\n\
                2. Order size too small (minimum may be required)\n\
                3. Price moved or insufficient liquidity\n\
                4. Market closed or token inactive",
                error_msg,
                response.order_id,
                token_id,
                side,
                amount_decimal,
                final_price_f64
            );
        }
    }
    
    /// Cancel an order by order ID
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let _private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order cancellation. Please set private_key in config.json"))?;
        
        let signer = LocalSigner::from_str(_private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy,
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        client.cancel_order(order_id).await
            .context(format!("Failed to cancel order {}", order_id))?;
        
        Ok(())
    }

    /// Check if both Up and Down orders are filled (production mode: verify via CLOB API).
    /// Returns Ok((up_filled, down_filled)). Order not found or API error is treated as not filled.
    pub async fn are_both_orders_filled(&self, up_order_id: &str, down_order_id: &str) -> Result<(bool, bool)> {
        let _private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key required to check order status"))?;

        let signer = LocalSigner::from_str(_private_key)
            .context("Failed to create signer from private key")?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);

        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}", proxy_addr))?;
            auth_builder = auth_builder.funder(funder_address);
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => SignatureType::Proxy,
                Some(n) => anyhow::bail!("Invalid signature_type: {}", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }

        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API")?;

        let up_filled = client.order(up_order_id).await
            .ok()
            .map(|o| o.status == OrderStatusType::Matched)
            .unwrap_or(false);

        let down_filled = client.order(down_order_id).await
            .ok()
            .map(|o| o.status == OrderStatusType::Matched)
            .unwrap_or(false);

        Ok((up_filled, down_filled))
    }
    
    #[allow(dead_code)]
    async fn place_order_hmac(&self, order: &OrderRequest) -> Result<OrderResponse> {
        let path = "/orders";
        let url = format!("{}{}", self.clob_url, path);
        
        let body = serde_json::to_string(order)
            .context("Failed to serialize order to JSON")?;
        
        let mut request = self.client.post(&url).json(order);
        
        request = self.add_auth_headers(request, "POST", path, &body)
            .context("Failed to add authentication headers")?;

        eprintln!("📤 Posting order to Polymarket (HMAC): {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);

        let response = request
            .send()
            .await
            .context("Failed to place order")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            
            if status == 401 || status == 403 {
                anyhow::bail!(
                    "Authentication failed (status: {}): {}\n\
                    Troubleshooting:\n\
                    1. Verify your API credentials (api_key, api_secret, api_passphrase) are correct\n\
                    2. Verify your private_key is correct (required for order signing)\n\
                    3. Check if your API key has trading permissions\n\
                    4. Ensure your account has sufficient balance",
                    status, error_text
                );
            }
            
            anyhow::bail!("Failed to place order (status: {}): {}", status, error_text);
        }

        let order_response: OrderResponse = response
            .json()
            .await
            .context("Failed to parse order response")?;

        eprintln!("✅ Order placed successfully: {:?}", order_response);
        Ok(order_response)
    }

    pub async fn get_redeemable_positions(&self, wallet: &str) -> Result<Vec<String>> {
        let url = "https://data-api.polymarket.com/positions";
        let user = if wallet.starts_with("0x") {
            wallet.to_string()
        } else {
            format!("0x{}", wallet)
        };
        let response = self.client
            .get(url)
            .query(&[("user", user.as_str()), ("redeemable", "true"), ("limit", "500")])
            .send()
            .await
            .context("Failed to fetch redeemable positions")?;
        if !response.status().is_success() {
            anyhow::bail!("Data API returned {} for redeemable positions", response.status());
        }
        let positions: Vec<Value> = response.json().await.unwrap_or_default();
        let mut condition_ids: Vec<String> = positions
            .iter()
            .filter(|p| {
                // Only include positions where the wallet actually holds tokens (size > 0)
                let size = p.get("size")
                    .and_then(|s| s.as_f64())
                    .or_else(|| p.get("size").and_then(|s| s.as_u64().map(|u| u as f64)))
                    .or_else(|| p.get("size").and_then(|s| s.as_str()).and_then(|s| s.parse::<f64>().ok()));
                size.map(|s| s > 0.0).unwrap_or(false)
            })
            .filter_map(|p| p.get("conditionId").and_then(|c| c.as_str()).map(|s| {
                if s.starts_with("0x") { s.to_string() } else { format!("0x{}", s) }
            }))
            .collect();
        condition_ids.sort();
        condition_ids.dedup();
        Ok(condition_ids)
    }

    pub async fn redeem_tokens(
        &self,
        condition_id: &str,
        _token_id: &str,
        outcome: &str,
    ) -> Result<RedeemResponse> {
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        let parse_address_hex = |s: &str| -> Result<Address> {
            let hex_str = s.strip_prefix("0x").unwrap_or(s);
            let bytes = hex::decode(hex_str).context("Invalid hex in address")?;
            let len= bytes.len();
            let arr: [u8; 20] = bytes.try_into().map_err(|_| anyhow::anyhow!("Address must be 20 bytes, got {}", len))?;
            Ok(Address::from(arr))
        };

        let collateral_token = parse_address_hex("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
            .context("Failed to parse USDC address")?;

        let condition_id_clean = condition_id.strip_prefix("0x").unwrap_or(condition_id);
        let condition_id_b256 = B256::from_str(condition_id_clean)
            .context(format!("Failed to parse condition_id as B256: {}", condition_id))?;

        let index_set = if outcome.to_uppercase().contains("UP") || outcome == "1" {
            U256::from(1)
        } else {
            U256::from(2)
        };

        eprintln!("Redeeming winning tokens for condition {} (outcome: {}, index_set: {})", 
              condition_id, outcome, index_set);
        
        const CTF_CONTRACT: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
        const RPC_URL: &str = "https://polygon-rpc.com";
        // Polymarket Proxy Wallet Factory (MagicLink users) – execute via factory.proxy([call])
        const PROXY_WALLET_FACTORY: &str = "0xaB45c5A4B0c941a2F231C04C3f49182e1A254052";
        
        let ctf_address = parse_address_hex(CTF_CONTRACT)
            .context("Failed to parse CTF contract address")?;
        
        let parent_collection_id = B256::ZERO;
        let use_proxy = self.proxy_wallet_address.is_some();
        let sig_type = self.signature_type.unwrap_or(1);
        // Gnosis Safe path: use index sets [1, 2] in one call (matches working new_redeem.py claim())
        let index_sets: Vec<U256> = if use_proxy && sig_type == 2 {
            vec![U256::from(1), U256::from(2)]
        } else {
            vec![index_set]
        };
        
        eprintln!("   Prepared redemption parameters:");
        eprintln!("   - CTF Contract: {}", ctf_address);
        eprintln!("   - Collateral token (USDC): {}", collateral_token);
        eprintln!("   - Condition ID: {} ({:?})", condition_id, condition_id_b256);
        eprintln!("   - Index set(s): {:?} (outcome: {})", index_sets, outcome);
        
        // Encode redeemPositions via alloy sol! (matches Polymarket rs-clob-client / Gnosis CTF ABI)
        let redeem_call = IConditionalTokens::redeemPositionsCall {
            collateralToken: collateral_token,
            parentCollectionId: parent_collection_id,
            conditionId: condition_id_b256,
            indexSets: index_sets.clone(),
        };
        let redeem_calldata = redeem_call.abi_encode();
        
        let (tx_to, tx_data, gas_limit, used_safe_redemption) = if use_proxy && sig_type == 2 {
            // Gnosis Safe: create Safe tx (redeemPositions), sign with EOA, execute via Safe.execTransaction
            // Matches redeem.ts redeemPositionsViaSafe() using Safe SDK (createTransaction -> signTransaction -> executeTransaction)
            let safe_address_str = self.proxy_wallet_address.as_deref()
                .ok_or_else(|| anyhow::anyhow!("proxy_wallet_address required for Safe redemption"))?;
            let safe_address = parse_address_hex(safe_address_str)
                .context("Failed to parse proxy_wallet_address (Safe address)")?;
            eprintln!("   Using Gnosis Safe (proxy): signing and executing redemption via Safe.execTransaction");
            // 1) Get Safe nonce
            let nonce_selector = keccak256("nonce()".as_bytes());
            let nonce_calldata: Vec<u8> = nonce_selector.as_slice()[..4].to_vec();
            let provider_read = ProviderBuilder::new()
                .connect(RPC_URL)
                .await
                .context("Failed to connect to RPC for Safe read calls")?;
            let nonce_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(nonce_calldata.clone()).into());
            let nonce_result = provider_read.call(nonce_tx).await
                .context("Failed to call Safe.nonce()")?;
            let nonce_bytes: [u8; 32] = nonce_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("Safe.nonce() did not return 32 bytes"))?;
            let nonce = U256::from_be_slice(&nonce_bytes);
            // safeTxGas: use non-zero like new_redeem.py (REDEEM_GAS_LIMIT). 0 can cause inner call to fail.
            const SAFE_TX_GAS: u64 = 300_000;
            // 2) Get transaction hash from Safe.getTransactionHash(to, value, data, operation, safeTxGas, baseGas, gasPrice, gasToken, refundReceiver, nonce)
            let get_tx_hash_sig = "getTransactionHash(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,uint256)";
            let get_tx_hash_selector = keccak256(get_tx_hash_sig.as_bytes()).as_slice()[..4].to_vec();
            let zero_addr = [0u8; 32];
            let mut to_enc = [0u8; 32];
            to_enc[12..].copy_from_slice(ctf_address.as_slice());
            let data_offset_get_hash = U256::from(32u32 * 10u32); // 320: data starts after 10 param words
            let mut get_tx_hash_calldata = Vec::new();
            get_tx_hash_calldata.extend_from_slice(&get_tx_hash_selector);
            get_tx_hash_calldata.extend_from_slice(&to_enc);
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&data_offset_get_hash.to_be_bytes::<32>());
            get_tx_hash_calldata.push(0); get_tx_hash_calldata.extend_from_slice(&[0u8; 31]); // operation = 0 (Call)
            get_tx_hash_calldata.extend_from_slice(&U256::from(SAFE_TX_GAS).to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&zero_addr);
            get_tx_hash_calldata.extend_from_slice(&zero_addr);
            get_tx_hash_calldata.extend_from_slice(&nonce.to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&U256::from(redeem_calldata.len()).to_be_bytes::<32>());
            get_tx_hash_calldata.extend_from_slice(&redeem_calldata);
            let get_tx_hash_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(get_tx_hash_calldata).into());
            let tx_hash_result = provider_read.call(get_tx_hash_tx).await
                .context("Failed to call Safe.getTransactionHash()")?;
            let tx_hash_to_sign: B256 = tx_hash_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("getTransactionHash did not return 32 bytes"))?;
            // 3) Sign with EIP-191 personal sign (same as new_redeem.py: encode_defunct(primitive=tx_hash) then sign_message).
            //    Hash to sign = keccak256("\x19E" + "thereum Signed Message:\n" + len_decimal + tx_hash)
            const EIP191_PREFIX: &[u8] = b"\x19Ethereum Signed Message:\n32";
            let mut eip191_message = Vec::with_capacity(EIP191_PREFIX.len() + 32);
            eip191_message.extend_from_slice(EIP191_PREFIX);
            eip191_message.extend_from_slice(tx_hash_to_sign.as_slice());
            let hash_to_sign = keccak256(&eip191_message);
            let sig = signer.sign_hash(&hash_to_sign).await
                .context("Failed to sign Safe transaction hash")?;
            let sig_bytes = sig.as_bytes();
            let r = &sig_bytes[0..32];
            let s = &sig_bytes[32..64];
            let v = sig_bytes[64];
            let v_safe = if v == 27 || v == 28 { v + 4 } else { v };
            let mut packed_sig: Vec<u8> = Vec::with_capacity(85);
            packed_sig.extend_from_slice(r);
            packed_sig.extend_from_slice(s);
            packed_sig.extend_from_slice(&[v_safe]);
            // Multi-sig format: if threshold > 1, prepend owner address (20 bytes) per new_redeem.py.
            let get_threshold_selector = keccak256("getThreshold()".as_bytes()).as_slice()[..4].to_vec();
            let threshold_tx = TransactionRequest::default()
                .to(safe_address)
                .input(Bytes::from(get_threshold_selector).into());
            let threshold_result = provider_read.call(threshold_tx).await
                .context("Failed to call Safe.getThreshold()")?;
            let threshold_bytes: [u8; 32] = threshold_result.as_ref().try_into()
                .map_err(|_| anyhow::anyhow!("getThreshold did not return 32 bytes"))?;
            let threshold = U256::from_be_slice(&threshold_bytes);
            if threshold > U256::from(1) {
                let owner = signer.address();
                let mut with_owner = Vec::with_capacity(20 + packed_sig.len());
                with_owner.extend_from_slice(owner.as_slice());
                with_owner.extend_from_slice(&packed_sig);
                packed_sig = with_owner;
            }
            let safe_sig_bytes = packed_sig;
            // 4) Encode execTransaction(to, value, data, operation, safeTxGas, baseGas, gasPrice, gasToken, refundReceiver, signatures)
            let exec_sig = "execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes)";
            let exec_selector = keccak256(exec_sig.as_bytes()).as_slice()[..4].to_vec();
            let data_offset = 32u32 * 10u32; // 320: first dynamic param starts after 10 words
            let sigs_offset = data_offset + 32 + redeem_calldata.len() as u32; // offset to signatures bytes
            let mut exec_calldata = Vec::new();
            exec_calldata.extend_from_slice(&exec_selector);
            exec_calldata.extend_from_slice(&to_enc);
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::from(data_offset).to_be_bytes::<32>());
            exec_calldata.push(0); exec_calldata.extend_from_slice(&[0u8; 31]);
            exec_calldata.extend_from_slice(&U256::from(SAFE_TX_GAS).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&zero_addr);
            exec_calldata.extend_from_slice(&zero_addr);
            exec_calldata.extend_from_slice(&U256::from(sigs_offset).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&U256::from(redeem_calldata.len()).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&redeem_calldata);
            exec_calldata.extend_from_slice(&U256::from(safe_sig_bytes.len()).to_be_bytes::<32>());
            exec_calldata.extend_from_slice(&safe_sig_bytes);
            (safe_address, exec_calldata, 400_000u64, true)
        } else if use_proxy && sig_type == 1 {
            // Polymarket Proxy: execute via Proxy Wallet Factory – factory.proxy([(typeCode, to, value, data)])
            // Refs: https://docs.polymarket.com/developers/proxy-wallet, Polymarket/examples examples/proxyWallet/redeem.ts
            eprintln!("   Using proxy wallet: sending redemption via Proxy Wallet Factory");
            let factory_address = parse_address_hex(PROXY_WALLET_FACTORY)
                .context("Failed to parse Proxy Wallet Factory address")?;
            // ABI: proxy((uint8 typeCode, address to, uint256 value, bytes data)[] calls)
            let selector = keccak256("proxy((uint8,address,uint256,bytes)[])".as_bytes());
            let proxy_selector = &selector.as_slice()[..4];
            // Encode one call: typeCode=1 (Call), to=CTF, value=0, data=redeem_calldata
            let mut proxy_calldata = Vec::with_capacity(4 + 32 * 3 + 128 + 32 + redeem_calldata.len());
            proxy_calldata.extend_from_slice(proxy_selector);
            // offset to array (params start at byte 4) = 32
            proxy_calldata.extend_from_slice(&U256::from(32u32).to_be_bytes::<32>());
            // array length = 1
            proxy_calldata.extend_from_slice(&U256::from(1u32).to_be_bytes::<32>());
            // offset to first tuple from start of params = 96 (tuple at 4+96=100)
            proxy_calldata.extend_from_slice(&U256::from(96u32).to_be_bytes::<32>());
            // tuple: typeCode = 1 (32 bytes, right-padded)
            let mut type_code = [0u8; 32];
            type_code[31] = 1;
            proxy_calldata.extend_from_slice(&type_code);
            // to = ctf_address (32 bytes, left-padded)
            let mut to_bytes = [0u8; 32];
            to_bytes[12..].copy_from_slice(ctf_address.as_slice());
            proxy_calldata.extend_from_slice(&to_bytes);
            // value = 0
            proxy_calldata.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
            // offset to bytes (from start of tuple) = 128
            proxy_calldata.extend_from_slice(&U256::from(128u32).to_be_bytes::<32>());
            // bytes: length then data
            let data_len = redeem_calldata.len();
            proxy_calldata.extend_from_slice(&U256::from(data_len).to_be_bytes::<32>());
            proxy_calldata.extend_from_slice(&redeem_calldata);
            (factory_address, proxy_calldata, 400_000u64, false)
        } else {
            // EOA or no proxy: send redeemPositions directly to CTF (tokens must be in EOA)
            eprintln!("   Sending redemption from EOA to CTF contract");
            (ctf_address, redeem_calldata, 300_000, false)
        };
        
        let provider = ProviderBuilder::new()
            .wallet(signer.clone())
            .connect(RPC_URL)
            .await
            .context("Failed to connect to Polygon RPC")?;
        
        let tx_request = TransactionRequest {
            to: Some(alloy::primitives::TxKind::Call(tx_to)),
            input: Bytes::from(tx_data).into(),
            value: Some(U256::ZERO),
            gas: Some(gas_limit),
            ..Default::default()
        };
        
        let pending_tx = match provider.send_transaction(tx_request).await {
            Ok(tx) => tx,
            Err(e) => {
                let err_msg = format!("Failed to send redeem transaction: {}", e);
                eprintln!("   {}", err_msg);
                anyhow::bail!("{}", err_msg);
            }
        };

        let tx_hash = *pending_tx.tx_hash();
        eprintln!("   Transaction sent, waiting for confirmation...");
        eprintln!("   Transaction hash: {:?}", tx_hash);
        
        let receipt = pending_tx.get_receipt().await
            .context("Failed to get transaction receipt")?;
        
        if !receipt.status() {
            anyhow::bail!("Redemption transaction failed. Transaction hash: {:?}", tx_hash);
        }
        
        // When using Gnosis Safe, the outer tx can succeed while the inner CTF redeemPositions reverts.
        // Detect inner failure by checking for CTF PayoutRedemption event in logs.
        if used_safe_redemption {
            let payout_redemption_topic = keccak256(
                b"PayoutRedemption(address,address,bytes32,bytes32,uint256[],uint256)"
            );
            let logs = receipt.logs();
            let ctf_has_payout = logs.iter().any(|log| {
                log.address() == ctf_address && log.topics().first().map(|t| t.as_slice()) == Some(payout_redemption_topic.as_slice())
            });
            if !ctf_has_payout {
                anyhow::bail!(
                    "Redemption tx was mined but the inner redeem reverted (no PayoutRedemption from CTF). \
                    Check that the Safe holds the winning tokens and conditionId/indexSet are correct. Tx: {:?}",
                    tx_hash
                );
            }
        }
        
        let redeem_response = RedeemResponse {
            success: true,
            message: Some(format!("Successfully redeemed tokens. Transaction: {:?}", tx_hash)),
            transaction_hash: Some(format!("{:?}", tx_hash)),
            amount_redeemed: None,
        };
        eprintln!("Successfully redeemed winning tokens!");
        eprintln!("Transaction hash: {:?}", tx_hash);
        if let Some(block_number) = receipt.block_number {
            eprintln!("Block number: {}", block_number);
        }
        Ok(redeem_response)
    }
}

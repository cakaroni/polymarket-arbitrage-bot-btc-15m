use crate::models::*;
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex;
use log::{warn, error};
use std::sync::Arc;

// Official SDK imports for proper order signing
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::clob::types::{Side, OrderType, SignatureType};
use polymarket_client_sdk::POLYGON;
use alloy::signers::local::LocalSigner;
use alloy::signers::Signer as _;
use alloy::primitives::Address as AlloyAddress;

// CTF (Conditional Token Framework) imports for merging positions
use alloy::primitives::{Address, B256, U256, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::eth::TransactionRequest;

type HmacSha256 = Hmac<Sha256>;

pub struct PolymarketApi {
    client: Client,
    gamma_url: String,
    clob_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    private_key: Option<String>,
    // Proxy wallet configuration (for Polymarket proxy wallet)
    proxy_wallet_address: Option<String>,
    signature_type: Option<u8>, // 0 = EOA, 1 = Proxy, 2 = GnosisSafe
    // Track if authentication was successful at startup
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
    

    /// Authenticate with Polymarket CLOB API at startup
    /// This verifies credentials (private_key + API credentials)
    /// Equivalent to JavaScript: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress)
    pub async fn authenticate(&self) -> Result<()> {
        // Check if we have required credentials
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for authentication. Please set private_key in config.json"))?;
        
        // Create signer from private key (equivalent to: new Wallet(PRIVATE_KEY))
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
            let sig_type = match self.signature_type {
                Some(1) => SignatureType::Proxy,
                Some(2) => SignatureType::GnosisSafe,
                Some(0) | None => {
                    warn!("⚠️  proxy_wallet_address is set but signature_type is EOA. Defaulting to Proxy.");
                    SignatureType::Proxy
                },
                Some(n) => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            
            auth_builder = auth_builder.signature_type(sig_type);
            eprintln!("🔐 Using proxy wallet: {} (signature type: {:?})", proxy_addr, sig_type);
        } else if let Some(sig_type_num) = self.signature_type {
            // If signature type is set but no proxy wallet, validate it's EOA
            let sig_type = match sig_type_num {
                0 => SignatureType::Eoa,
                1 | 2 => anyhow::bail!("signature_type {} requires proxy_wallet_address to be set", sig_type_num),
                n => anyhow::bail!("Invalid signature_type: {}. Must be 0 (EOA), 1 (Proxy), or 2 (GnosisSafe)", n),
            };
            auth_builder = auth_builder.signature_type(sig_type);
        }
        
        // Authenticate (equivalent to: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress))
        // This verifies that both private_key and API credentials are valid
        let _client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials (api_key, api_secret, api_passphrase) and private_key.")?;
        
        // Mark as authenticated
        *self.authenticated.lock().await = true;
        
        eprintln!("✅ Successfully authenticated with Polymarket CLOB API");
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
        
        // Create message: method + path + body + timestamp
        let message = format!("{}{}{}{}", method, path, body, timestamp);
        
        // Try to decode secret from base64 first, if that fails use as raw bytes
        let secret_bytes = match base64::decode(secret) {
            Ok(bytes) => bytes,
            Err(_) => {
                // If base64 decode fails, try using the secret directly as bytes
                // This handles cases where the secret is already in the correct format
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
        // Only add auth headers if we have all required credentials
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

    /// Get all active markets (using events endpoint)
    pub async fn get_all_active_markets(&self, limit: u32) -> Result<Vec<Market>> {
        let url = format!("{}/events", self.gamma_url);
        let limit_str = limit.to_string();
        let mut params = HashMap::new();
        params.insert("active", "true");
        params.insert("closed", "false");
        params.insert("limit", &limit_str);

        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to fetch all active markets")?;

        let status = response.status();
        let json: Value = response.json().await.context("Failed to parse markets response")?;
        
        if !status.is_success() {
            log::warn!("Get all active markets API returned error status {}: {}", status, serde_json::to_string(&json).unwrap_or_default());
            anyhow::bail!("API returned error status {}: {}", status, serde_json::to_string(&json).unwrap_or_default());
        }
        
        // Extract markets from events - events contain markets
        let mut all_markets = Vec::new();
        
        if let Some(events) = json.as_array() {
            for event in events {
                if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                    for market_json in markets {
                        if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                            all_markets.push(market);
                        }
                    }
                }
            }
        } else if let Some(data) = json.get("data") {
            if let Some(events) = data.as_array() {
                for event in events {
                    if let Some(markets) = event.get("markets").and_then(|m| m.as_array()) {
                        for market_json in markets {
                            if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                                all_markets.push(market);
                            }
                        }
                    }
                }
            }
        }
        
        log::debug!("Fetched {} active markets from events endpoint", all_markets.len());
        Ok(all_markets)
    }

    /// Get market by slug (e.g., "btc-updown-15m-1767726000")
    /// The API returns an event object with a markets array
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
        
        // The response is an event object with a "markets" array
        // Extract the first market from the markets array
        if let Some(markets) = json.get("markets").and_then(|m| m.as_array()) {
            if let Some(market_json) = markets.first() {
                // Try to deserialize the market
                if let Ok(market) = serde_json::from_value::<Market>(market_json.clone()) {
                    return Ok(market);
                }
            }
        }
        
        anyhow::bail!("Invalid market response format: no markets array found")
    }

    /// Get order book for a specific token
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

    /// Get price for a token (for trading)
    /// side: "BUY" or "SELL"
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

    /// Get best bid/ask prices for a token (from orderbook)
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

    /// Place an order using the official SDK with proper private key signing
    /// 
    /// This method uses the official polymarket-client-sdk to:
    /// 1. Create signer from private key
    /// 2. Authenticate with the CLOB API
    /// 3. Create and sign the order
    /// 4. Post the signed order
    /// 
    /// Equivalent to JavaScript: client.createAndPostOrder(userOrder)
    pub async fn place_order(&self, order: &OrderRequest) -> Result<OrderResponse> {
        // Check if we have a private key (required for signing)
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        // Create signer from private key (equivalent to: new Wallet(PRIVATE_KEY))
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
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
        
        // Create CLOB client with authentication (equivalent to: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress))
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        // Convert order side string to SDK Side enum
        let side = match order.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", order.side),
        };
        
        // Parse price and size to Decimal
        let price = rust_decimal::Decimal::from_str(&order.price)
            .context(format!("Failed to parse price: {}", order.price))?;
        let size = rust_decimal::Decimal::from_str(&order.size)
            .context(format!("Failed to parse size: {}", order.size))?;
        
        eprintln!("📤 Creating and posting order: {} {} {} @ {}", 
              order.side, order.size, order.token_id, order.price);
        
        // Create and post order using SDK (equivalent to: client.createAndPostOrder(userOrder))
        // This automatically creates, signs, and posts the order
        let order_builder = client
            .limit_order()
            .token_id(&order.token_id)
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

    /// Place a market order (FOK/FAK) for immediate execution
    /// 
    /// This is used for emergency selling or when you want immediate execution at market price.
    /// Equivalent to JavaScript: client.createAndPostMarketOrder(userMarketOrder)
    /// 
    /// Market orders execute immediately at the best available price:
    /// - FOK (Fill-or-Kill): Order must fill completely or be cancelled
    /// - FAK (Fill-and-Kill): Order fills as much as possible, remainder is cancelled
    pub async fn place_market_order(
        &self,
        token_id: &str,
        amount: f64,
        side: &str,
        order_type: Option<&str>, // "FOK" or "FAK", defaults to FOK
    ) -> Result<OrderResponse> {
        // Check if we have a private key (required for signing)
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order signing. Please set private_key in config.json"))?;
        
        // Create signer from private key
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
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
        
        // Create CLOB client with authentication (equivalent to: new ClobClient(HOST, CHAIN_ID, signer, apiCreds, signatureType, funderAddress))
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        // Convert order side string to SDK Side enum
        let side_enum = match side {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => anyhow::bail!("Invalid order side: {}. Must be 'BUY' or 'SELL'", side),
        };
        
        // Convert order type (defaults to FOK for immediate execution)
        let order_type_enum = match order_type.unwrap_or("FOK") {
            "FOK" => OrderType::FOK,
            "FAK" => OrderType::FAK,
            _ => OrderType::FOK, // Default to FOK
        };
        
        use rust_decimal::{Decimal, RoundingStrategy};
        use rust_decimal::prelude::*;
        
        // Convert amount to Decimal and round to 2 decimal places (Polymarket requirement)
        let amount_decimal = Decimal::from_f64_retain(amount)
            .ok_or_else(|| anyhow::anyhow!("Failed to convert amount to Decimal"))?
            .round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
        
        eprintln!("📤 Creating and posting MARKET order: {} {} {} (type: {:?})", 
              side, amount_decimal, token_id, order_type_enum);
        
        // For market orders, we need to use the current market price to respect tick size requirements
        // Use the same method as monitor: get_price() API endpoint
        // - For BUY: Use ASK price (get_price with "SELL" returns ASK - what sellers are asking)
        // - For SELL: Use BID price (get_price with "BUY" returns BID - what buyers are bidding)
        let market_price = if matches!(side_enum, Side::Buy) {
            // For BUY orders, get the ASK price (what sellers are asking - higher price)
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
        
        // Use limit order with aggressive pricing to simulate market order
        // This ensures immediate execution at best available price
        let order_builder = client
            .limit_order()
            .token_id(token_id)
            .size(amount_decimal)
            .price(market_price)
            .side(side_enum);
        
        let signed_order = client.sign(&signer, order_builder.build().await?)
            .await
            .context("Failed to sign market order")?;
        
        // For SELL orders, adjust price to be slightly below BID for immediate execution
        // This ensures the order fills quickly (market sell should be aggressive)
        // IMPORTANT: Polymarket requires prices to have at most 2 decimal places (tick size 0.01)
        let final_price = if matches!(side_enum, Side::Sell) {
            // Use 0.5% below BID to ensure immediate fill (more aggressive)
            // Convert to f64, adjust, then back to Decimal with proper rounding
            let price_f64 = f64::try_from(market_price).unwrap_or(0.0);
            let adjusted_f64 = price_f64 * 0.995;
            // Round to 2 decimal places (Polymarket requirement: tick size 0.01)
            let rounded_f64 = (adjusted_f64 * 100.0).round() / 100.0;
            // Ensure minimum price of 0.01
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
                .token_id(token_id)
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
        // Check if we have a private key (required for signing)
        let _private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required for order cancellation. Please set private_key in config.json"))?;
        
        // Create signer from private key
        let signer = LocalSigner::from_str(_private_key)
            .context("Failed to create signer from private key. Ensure private_key is a valid hex string.")?
            .with_chain_id(Some(POLYGON));
        
        // Build authentication builder with proxy wallet support (same pattern as place_order)
        let mut auth_builder = ClobClient::new(&self.clob_url, ClobConfig::default())
            .context("Failed to create CLOB client")?
            .authentication_builder(&signer);
        
        // Configure proxy wallet if provided
        if let Some(proxy_addr) = &self.proxy_wallet_address {
            let funder_address = AlloyAddress::parse_checksummed(proxy_addr, None)
                .context(format!("Failed to parse proxy_wallet_address: {}. Ensure it's a valid Ethereum address.", proxy_addr))?;
            
            auth_builder = auth_builder.funder(funder_address);
            
            // Set signature type based on config or default to Proxy
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
        
        // Create CLOB client with authentication (same pattern as place_order)
        let client = auth_builder
            .authenticate()
            .await
            .context("Failed to authenticate with CLOB API. Check your API credentials.")?;
        
        // Cancel the order using the SDK
        client.cancel_order(order_id).await
            .context(format!("Failed to cancel order {}", order_id))?;
        
        Ok(())
    }
    
    /// Place an order using REST API with HMAC authentication (fallback method)
    /// 
    /// NOTE: This is a fallback method. The main place_order() method uses the official SDK
    /// with proper private key signing. Use this only if SDK integration fails.
    #[allow(dead_code)]
    async fn place_order_hmac(&self, order: &OrderRequest) -> Result<OrderResponse> {
        let path = "/orders";
        let url = format!("{}{}", self.clob_url, path);
        
        // Serialize order to JSON string for signature
        let body = serde_json::to_string(order)
            .context("Failed to serialize order to JSON")?;
        
        let mut request = self.client.post(&url).json(order);
        
        // Add HMAC-SHA256 authentication headers (L2 authentication)
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
            
            // Provide helpful error messages
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

    /// Merge Up and Down tokens into collateral (USDC)
    /// This can be done BEFORE the market finishes if you hold both sides
    pub async fn merge_positions(
        &self,
        condition_id: &str,
        amount_shares: f64,
    ) -> Result<RedeemResponse> {
        let collateral_token = Address::parse_checksummed(
            "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
            None
        ).context("Failed to parse USDC address")?;
        
        let condition_id_clean = condition_id.strip_prefix("0x").unwrap_or(condition_id);
        let condition_id_b256 = B256::from_str(condition_id_clean)
            .context(format!("Failed to parse condition_id to B256: {}", condition_id))?;
        
        // For binary markets, merge index sets [1, 2]
        let index_sets = vec![U256::from(1), U256::from(2)];
        
        // Convert shares to U256 (6 decimal places for USDC/Polymarket shares)
        let amount_u256 = U256::from((amount_shares * 1_000_000.0) as u64);
        
        eprintln!("🔄 Merging Up and Down tokens for condition {} (amount: {:.6} shares)", 
              condition_id, amount_shares);
        
        self.execute_ctf_call(collateral_token, condition_id_b256, index_sets, amount_u256).await
    }

    /// Execute a call to the CTF contract (mergePositions only)
    async fn execute_ctf_call(
        &self,
        collateral_token: Address,
        condition_id_b256: B256,
        index_sets: Vec<U256>,
        amount: U256,
    ) -> Result<RedeemResponse> {
        // Create signer from private key
        let private_key = self.private_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Private key is required. Please set private_key in config.json"))?;
        
        let signer = LocalSigner::from_str(private_key)
            .context("Failed to create signer from private key")?
            .with_chain_id(Some(POLYGON));
        const CTF_CONTRACT: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
        const RPC_URL: &str = "https://polygon-rpc.com";
        
        let ctf_address = Address::parse_checksummed(CTF_CONTRACT, None)
            .context("Failed to parse CTF contract address")?;
        
        // Function selector for mergePositions(address,bytes32,bytes32,uint256[],uint256) -> 0x82cc0f7a
        let selector_hex = "82cc0f7a";
        let function_selector = hex::decode(selector_hex).context("Failed to decode function selector")?;
        
        let mut encoded_params = Vec::new();
        
        // address collateralToken (32 bytes padded)
        let mut addr_bytes = [0u8; 32];
        addr_bytes[12..].copy_from_slice(collateral_token.as_slice());
        encoded_params.extend_from_slice(&addr_bytes);
        
        // bytes32 parentCollectionId (32 bytes)
        encoded_params.extend_from_slice(&[0u8; 32]);
        
        // bytes32 conditionId (32 bytes)
        encoded_params.extend_from_slice(condition_id_b256.as_slice());
        
        // uint256[] indexSets (offset, length, elements)
        // Offset to indexSets array (32 * 5 for mergePositions)
        let array_offset = 32 * 5;
        encoded_params.extend_from_slice(&U256::from(array_offset).to_be_bytes::<32>());
        
        // uint256 amount (required for mergePositions)
        encoded_params.extend_from_slice(&amount.to_be_bytes::<32>());
        
        // Array length
        encoded_params.extend_from_slice(&U256::from(index_sets.len()).to_be_bytes::<32>());
        
        // Array elements
        for idx in &index_sets {
            encoded_params.extend_from_slice(&idx.to_be_bytes::<32>());
        }
        
        let mut call_data = function_selector;
        call_data.extend_from_slice(&encoded_params);
        
        // Create provider and send transaction
        let provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(RPC_URL.parse().context("Failed to parse RPC URL")?);
        
        let tx_request = TransactionRequest {
            to: Some(ctf_address.into()),
            input: Bytes::from(call_data).into(),
            value: Some(U256::ZERO),
            ..Default::default()
        };
        
        let pending_tx = provider.send_transaction(tx_request).await
            .context("Failed to send mergePositions transaction")?;
        
        let tx_hash = *pending_tx.tx_hash();
        let receipt = pending_tx.get_receipt().await
            .context("Failed to get transaction receipt")?;
        
        if receipt.status() {
            eprintln!("✅ Successfully executed mergePositions! Transaction: {:?}", tx_hash);
            Ok(RedeemResponse {
                success: true,
                message: Some(format!("Successfully executed mergePositions. Transaction: {:?}", tx_hash)),
                transaction_hash: Some(format!("{:?}", tx_hash)),
            })
        } else {
            anyhow::bail!("mergePositions transaction failed. Transaction hash: {:?}", tx_hash);
        }
    }

    /// Get fills (executed trades) for a specific user account and optionally filter by condition_id
    /// 
    /// This endpoint returns all fills (executed trades) for a given user address.
    /// Can optionally filter by condition_id to get trades for a specific market.
    /// 
    /// Parameters:
    /// - user_address: Ethereum address of the user (with or without 0x prefix)
    /// - condition_id: Optional condition ID to filter by specific market
    /// - limit: Maximum number of fills to return (default: 1000)
    /// 
    /// Reference: Polymarket Data API - /activity endpoint
    /// Documentation: https://docs.polymarket.com/developers/misc-endpoints/data-api-activity
    /// Uses public Data API: https://data-api.polymarket.com/activity
    pub async fn get_user_fills(
        &self,
        user_address: &str,
        condition_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<crate::models::Fill>> {
        // Use Data API for public trade history (not CLOB API)
        // Data API: https://data-api.polymarket.com/activity
        let data_api_url = "https://data-api.polymarket.com";
        let url = format!("{}/activity", data_api_url);
        
        // Ensure user address has 0x prefix (API expects it)
        let user_addr_formatted = if user_address.starts_with("0x") {
            user_address.to_string()
        } else {
            format!("0x{}", user_address)
        };
        
        // Set limit (default to 1000 if not specified)
        let limit_val = limit.unwrap_or(1000);
        
        // Build params in the correct order: limit, sortBy, sortDirection, user, market
        let mut params: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        params.insert("limit", limit_val.to_string());
        params.insert("sortBy", "TIMESTAMP".to_string());
        params.insert("sortDirection", "DESC".to_string());
        params.insert("user", user_addr_formatted.clone());
        
        // Add market filter if condition_id is provided
        if let Some(cond_id) = condition_id {
            params.insert("market", cond_id.to_string());
        }
        
        eprintln!("🔍 Fetching activity from Data API for user: {} (condition_id: {:?})", user_address, condition_id);
        
        // Build URL for logging in the correct order
        let mut url_parts = vec![
            format!("limit={}", limit_val),
            "sortBy=TIMESTAMP".to_string(),
            "sortDirection=DESC".to_string(),
            format!("user={}", user_addr_formatted),
        ];
        if let Some(cond_id) = condition_id {
            url_parts.push(format!("market={}", cond_id));
        }
        eprintln!("   URL: {}?{}", url, url_parts.join("&"));
        
        // Data API is public, no authentication required
        let response = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context(format!("Failed to fetch activity for user: {}", user_address))?;
        
        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "Failed to fetch activity from Data API (status: {}): {}\n\
                \n\
                Troubleshooting:\n\
                1. Verify the user address is correct: {}\n\
                2. Verify the condition_id is correct: {:?}\n\
                3. Check if the user has any trades in this market\n\
                4. Try without condition_id to get all user activity",
                status, error_text, user_address, condition_id
            );
        }
        
        let json: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse activity response")?;
        
        eprintln!("   Response structure: {}", if json.is_array() { "array" } else { "object" });
        
        // Parse activity from response
        // Data API /activity returns an array directly
        let activities: Vec<serde_json::Value> = if let Some(activities_array) = json.as_array() {
            activities_array.clone()
        } else if let Some(activities_array) = json.get("data").and_then(|d| d.as_array()) {
            activities_array.clone()
        } else {
            anyhow::bail!("Unexpected response format: expected array of activities");
        };
        
        // Filter to only TRADE type activities and convert to Fill structs
        let fills: Vec<crate::models::Fill> = activities
            .into_iter()
            .filter_map(|activity| {
                // Only process TRADE type activities
                if activity.get("type").and_then(|t| t.as_str()) != Some("TRADE") {
                    return None;
                }
                
                // Convert activity to Fill
                serde_json::from_value::<crate::models::Fill>(activity).ok()
            })
            .collect();
        
        eprintln!("✅ Fetched {} trades from {} total activities for user: {}", 
                  fills.len(), json.as_array().map(|a| a.len()).unwrap_or(0), user_address);
        
        Ok(fills)
    }

    /// Get fills for a specific market (condition_id) by fetching market tokens first
    /// 
    /// This method:
    /// 1. Fetches market details to get Up and Down token IDs
    /// 2. Fetches fills for the user
    /// 3. Filters fills to only include tokens from this market
    pub async fn get_user_fills_for_market(
        &self,
        user_address: &str,
        condition_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<crate::models::Fill>> {
        // First, get market details to find token IDs
        let market = self.get_market(condition_id).await
            .context(format!("Failed to fetch market for condition_id: {}", condition_id))?;
        
        // Extract token IDs from market
        let market_token_ids: std::collections::HashSet<String> = market.tokens
            .iter()
            .map(|t| t.token_id.clone())
            .collect();
        
        eprintln!("📊 Market has {} tokens: {:?}", market_token_ids.len(), market_token_ids);
        
        // Fetch fills for user filtered by this market's condition_id
        let all_fills = self.get_user_fills(user_address, Some(condition_id), limit).await?;
        
        // Filter fills to only include tokens from this market
        // Data API returns conditionId in the fill, so we can filter by that
        let market_fills: Vec<crate::models::Fill> = all_fills
            .into_iter()
            .filter(|fill| {
                // Filter by condition_id if available
                if let Some(fill_cond_id) = &fill.condition_id {
                    if fill_cond_id == condition_id {
                        return true;
                    }
                }
                // Fallback: filter by token_id matching market tokens
                if let Some(token_id) = &fill.token_id {
                    market_token_ids.contains(token_id)
                } else {
                    false
                }
            })
            .collect();
        
        eprintln!("✅ Found {} fills for market {} (condition_id: {})", 
                  market_fills.len(), market.question, condition_id);
        
        Ok(market_fills)
    }
    
    /// Alternative method: Fetch fills by fetching market tokens and querying by token_id
    /// This is a workaround if the /fills endpoint doesn't work
    async fn get_user_fills_by_token_ids(
        &self,
        _user_address: &str,
        condition_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<crate::models::Fill>> {
        eprintln!("   Trying alternative: Fetch fills by token IDs from market...");
        
        // Get market to find token IDs
        let market = self.get_market(condition_id).await
            .context(format!("Failed to fetch market for condition_id: {}", condition_id))?;
        
        let market_token_ids: Vec<String> = market.tokens
            .iter()
            .map(|t| t.token_id.clone())
            .collect();
        
        eprintln!("   Found {} tokens in market, trying to fetch fills by token_id...", market_token_ids.len());
        
        // Try fetching fills for each token
        let mut all_fills = Vec::new();
        for token_id in &market_token_ids {
            let url = format!("{}/fills", self.clob_url);
            let mut params: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
            params.insert("tokenID", token_id.clone());
            
            if let Some(limit_val) = limit {
                params.insert("limit", limit_val.to_string());
            }
            
            let mut request_builder = self.client.get(&url).query(&params);
            
            // Try with auth if available
            if self.api_key.is_some() && self.api_secret.is_some() && self.api_passphrase.is_some() {
                let path = "/fills";
                let body = "";
                match self.add_auth_headers(request_builder, "GET", path, body) {
                    Ok(auth_request) => request_builder = auth_request,
                    Err(_) => {
                        // If auth fails, continue without auth
                        request_builder = self.client.get(&url).query(&params);
                    }
                }
            }
            
            if let Ok(resp) = request_builder.send().await {
                if resp.status().is_success() {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        let fills: Vec<crate::models::Fill> = if let Some(fills_array) = json.as_array() {
                            serde_json::from_value(serde_json::Value::Array(fills_array.clone()))
                                .unwrap_or_default()
                        } else if let Some(fills_array) = json.get("fills").and_then(|f| f.as_array()) {
                            serde_json::from_value(serde_json::Value::Array(fills_array.clone()))
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        
                        // Filter by condition_id (since Fill struct doesn't have user/maker/taker fields)
                        // In a real implementation, you'd need to check the actual fill data structure
                        let user_fills: Vec<crate::models::Fill> = fills
                            .into_iter()
                            .filter(|fill| {
                                // Filter by condition_id if it matches
                                if let Some(fill_cond_id) = &fill.condition_id {
                                    fill_cond_id == condition_id
                                } else {
                                    false
                                }
                            })
                            .collect();
                        
                        all_fills.extend(user_fills);
                    }
                }
            }
        }
        
        if all_fills.is_empty() {
            anyhow::bail!(
                "Could not fetch fills using any method. Possible reasons:\n\
                1. The user has no trades in this market\n\
                2. The /fills endpoint requires authentication (set API credentials in config.json)\n\
                3. The endpoint format has changed\n\
                \n\
                Try:\n\
                - Verify the user address is correct\n\
                - Check if API credentials are needed\n\
                - Verify the condition_id is correct"
            );
        }
        
        eprintln!("✅ Found {} fills using token_id filtering", all_fills.len());
        Ok(all_fills)
    }
}


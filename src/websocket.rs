use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};

const WSS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WssAuth {
    #[serde(rename = "apiKey")]
    api_key: Option<String>,
    #[serde(rename = "apiSecret")]
    api_secret: Option<String>,
    #[serde(rename = "apiPassphrase")]
    api_passphrase: Option<String>,
    signature: Option<String>,
    timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WssSubscribeMessage {
    #[serde(rename = "assets_ids")]
    assets_ids: Vec<String>,
    r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WssUpdateMessage {
    #[serde(rename = "assets_ids")]
    assets_ids: Vec<String>,
    operation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BestBidAskMessage {
    #[serde(rename = "event_type")]
    event_type: String,
    market: String,
    #[serde(rename = "asset_id")]
    asset_id: String,
    #[serde(rename = "best_bid")]
    best_bid: String,
    #[serde(rename = "best_ask")]
    best_ask: String,
    spread: String,
    timestamp: String,
}

#[derive(Debug, Clone)]
pub struct TokenPriceUpdate {
    pub token_id: String,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub timestamp: u64,
}

pub struct WebSocketPriceManager {
    prices: Arc<RwLock<HashMap<String, TokenPriceUpdate>>>,
    subscribed_tokens: Arc<RwLock<Vec<String>>>,
}

impl WebSocketPriceManager {
    pub fn new() -> Self {
        Self {
            prices: Arc::new(RwLock::new(HashMap::new())),
            subscribed_tokens: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn get_price(&self, token_id: &str, side: &str) -> Option<Decimal> {
        let prices = self.prices.read().await;
        if let Some(update) = prices.get(token_id) {
            match side {
                "BUY" => Some(update.best_ask),  // For BUY, use ask price (what sellers are asking)
                "SELL" => Some(update.best_bid), // For SELL, use bid price (what buyers are bidding)
                _ => None,
            }
        } else {
            None
        }
    }

    pub async fn subscribe_to_tokens(&self, token_ids: Vec<String>) -> Result<()> {
        let mut subscribed = self.subscribed_tokens.write().await;
        for token_id in &token_ids {
            if !subscribed.contains(token_id) {
                subscribed.push(token_id.clone());
            }
        }
        Ok(())
    }

    pub async fn start(&self) -> Result<()> {
        let prices = Arc::clone(&self.prices);
        let subscribed_tokens = Arc::clone(&self.subscribed_tokens);

        tokio::spawn(async move {
            loop {
                match Self::connect_and_listen(prices.clone(), subscribed_tokens.clone()).await {
                    Ok(_) => {
                        warn!("WebSocket connection closed, will reconnect in 5 seconds...");
                    }
                    Err(e) => {
                        error!("WebSocket error: {}, will reconnect in 5 seconds...", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(())
    }

    async fn connect_and_listen(
        prices: Arc<RwLock<HashMap<String, TokenPriceUpdate>>>,
        subscribed_tokens: Arc<RwLock<Vec<String>>>,
    ) -> Result<()> {
        let url = url::Url::parse(WSS_URL)
            .context("Failed to parse WebSocket URL")?;

        info!("🔌 Connecting to Polymarket WebSocket: {}", WSS_URL);
        let (ws_stream, _) = connect_async(url)
            .await
            .context("Failed to connect to WebSocket")?;

        info!("✅ Connected to Polymarket WebSocket");

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to MARKET channel (initial subscription with empty assets_ids)
        let subscribe_msg = WssSubscribeMessage {
            assets_ids: vec![], // Will subscribe to specific tokens after connection
            r#type: "market".to_string(),
        };

        let subscribe_json = serde_json::to_string(&subscribe_msg)
            .context("Failed to serialize subscribe message")?;

        write.send(Message::Text(subscribe_json)).await
            .context("Failed to send subscribe message")?;

        info!("📡 Subscribed to MARKET channel");

        // Start ping interval (every 5 seconds as recommended)
        let mut ping_interval = interval(Duration::from_secs(5));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Check for new tokens to subscribe (every 10 seconds)
        let mut subscription_check_interval = interval(Duration::from_secs(10));
        subscription_check_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_subscribed_count = 0;

        // Initial subscription to tokens we're monitoring
        let tokens_to_subscribe: Vec<String> = {
            let tokens = subscribed_tokens.read().await;
            tokens.clone()
        };

        if !tokens_to_subscribe.is_empty() {
            let update_msg = WssUpdateMessage {
                assets_ids: tokens_to_subscribe.clone(),
                operation: "subscribe".to_string(),
            };

            let update_json = serde_json::to_string(&update_msg)
                .context("Failed to serialize update message")?;

            write.send(Message::Text(update_json)).await
                .context("Failed to send token subscription")?;

            last_subscribed_count = tokens_to_subscribe.len();
            debug!("📡 Subscribed to {} tokens", tokens_to_subscribe.len());
        }

        // Listen for messages
        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    // Send PING to keep connection alive (as text message "PING")
                    if let Err(e) = write.send(Message::Text("PING".to_string())).await {
                        error!("Failed to send PING: {}", e);
                        break;
                    }
                }
                _ = subscription_check_interval.tick() => {
                    // Check for new tokens to subscribe
                    let current_tokens: Vec<String> = {
                        let tokens = subscribed_tokens.read().await;
                        tokens.clone()
                    };
                    
                    if current_tokens.len() > last_subscribed_count {
                        let new_tokens: Vec<String> = current_tokens[last_subscribed_count..].to_vec();
                        let update_msg = WssUpdateMessage {
                            assets_ids: new_tokens.clone(),
                            operation: "subscribe".to_string(),
                        };
                        
                        if let Ok(update_json) = serde_json::to_string(&update_msg) {
                            if let Err(e) = write.send(Message::Text(update_json)).await {
                                error!("Failed to send token subscription: {}", e);
                            } else {
                                last_subscribed_count = current_tokens.len();
                                debug!("📡 Subscribed to {} new tokens", new_tokens.len());
                            }
                        }
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = Self::handle_message(&text, prices.clone()).await {
                                debug!("Error handling message: {}", e);
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            // Pong received, connection is alive
                        }
                        Some(Ok(Message::Close(_))) => {
                            info!("WebSocket connection closed by server");
                            break;
                        }
                        Some(Err(e)) => {
                            error!("WebSocket error: {}", e);
                            break;
                        }
                        None => {
                            info!("WebSocket stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_message(
        text: &str,
        prices: Arc<RwLock<HashMap<String, TokenPriceUpdate>>>,
    ) -> Result<()> {
        // Try to parse as best_bid_ask message
        if let Ok(msg) = serde_json::from_str::<BestBidAskMessage>(text) {
            if msg.event_type == "best_bid_ask" {
                let best_bid = Decimal::from_str(&msg.best_bid)
                    .context(format!("Failed to parse best_bid: {}", msg.best_bid))?;
                let best_ask = Decimal::from_str(&msg.best_ask)
                    .context(format!("Failed to parse best_ask: {}", msg.best_ask))?;
                let timestamp = msg.timestamp.parse::<u64>()
                    .context(format!("Failed to parse timestamp: {}", msg.timestamp))?;

                let asset_id = msg.asset_id.clone();
                let update = TokenPriceUpdate {
                    token_id: asset_id.clone(),
                    best_bid,
                    best_ask,
                    timestamp,
                };

                let mut price_map = prices.write().await;
                price_map.insert(asset_id.clone(), update);

                debug!("📊 Price update: token_id={}, bid={}, ask={}", 
                    asset_id, msg.best_bid, msg.best_ask);
            }
        }

        Ok(())
    }
}

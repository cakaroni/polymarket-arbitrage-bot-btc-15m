use crate::api::PolymarketApi;
use crate::config::Config;
use crate::discovery::MarketDiscovery;
use crate::models::*;
use crate::websocket::WebSocketPriceManager;
use anyhow::Result;
use chrono::Utc;
use chrono_tz::America::New_York;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

pub struct PreLimitStrategy {
    api: Arc<PolymarketApi>,
    config: Config,
    discovery: MarketDiscovery,
    states: Arc<Mutex<HashMap<String, PreLimitOrderState>>>, // Key: asset (BTC, ETH, etc.)
    last_status_display: Arc<Mutex<std::time::Instant>>,
    total_profit: Arc<Mutex<f64>>, // Accumulated profit from all merged positions
    ws_manager: Arc<WebSocketPriceManager>, // WebSocket price manager
}

impl PreLimitStrategy {
    pub fn new(api: Arc<PolymarketApi>, config: Config) -> Self {
        let discovery = MarketDiscovery::new(api.clone());
        let ws_manager = Arc::new(WebSocketPriceManager::new());
        Self {
            api,
            config,
            discovery,
            states: Arc::new(Mutex::new(HashMap::new())),
            last_status_display: Arc::new(Mutex::new(std::time::Instant::now())),
            total_profit: Arc::new(Mutex::new(0.0)),
            ws_manager,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Start WebSocket connection for real-time price updates
        self.ws_manager.start().await?;
        log::info!("🔌 WebSocket price manager started");
        
        // Wait a moment for WebSocket to connect
        sleep(Duration::from_secs(2)).await;
        
        // Initial market discovery and display
        self.display_market_status().await?;
        
        loop {
            let should_display = {
                let mut last = self.last_status_display.lock().await;
                if last.elapsed().as_secs() >= 10 {
                    *last = std::time::Instant::now();
                    true
                } else {
                    false
                }
            };
            
            if should_display {
                if let Err(e) = self.display_market_status().await {
                    log::error!("Error displaying market status: {}", e);
                }
            }
            
            if let Err(e) = self.process_markets().await {
                log::error!("Error processing markets: {}", e);
            }
            sleep(Duration::from_millis(self.config.strategy.check_interval_ms)).await;
        }
    }

    async fn process_markets(&self) -> Result<()> {
        let assets = vec!["BTC", "ETH", "SOL", "XRP"];
        let current_period_et = Self::get_current_15m_period_et();
        
        for asset in assets {
            self.process_asset(asset, current_period_et).await?;
        }
        Ok(())
    }
    
    /// Calculate the current running 15-minute period timestamp in ET timezone
    fn get_current_15m_period_et() -> i64 {
        let now_utc = Utc::now();
        let now_et = now_utc.with_timezone(&New_York);
        
        // Get seconds since epoch in ET
        let et_timestamp = now_et.timestamp();
        
        // Round down to nearest 15-minute boundary (900 seconds)
        (et_timestamp / 900) * 900
    }
    
    /// Get current time in ET timezone (not rounded to period)
    fn get_current_time_et() -> i64 {
        let now_utc = Utc::now();
        let now_et = now_utc.with_timezone(&New_York);
        now_et.timestamp()
    }

    async fn process_asset(&self, asset: &str, current_period_et: i64) -> Result<()> {
        let mut states = self.states.lock().await;
        let state = states.get(asset).cloned();
        
        let current_time_et = Self::get_current_time_et();
        let next_period_start = current_period_et + 900;
        let time_until_next = next_period_start - current_time_et;

        if time_until_next <= (self.config.strategy.place_order_before_mins * 60) as i64 {
            // Check if we already have a state for the next period
            let is_next_market_prepared = state.as_ref().map_or(false, |s| s.expiry == next_period_start + 900);
            
            if !is_next_market_prepared {
                log::info!("Preparing orders for next {} market (starts in {}s)", asset, time_until_next);
                if let Some(next_market) = self.discover_next_market(asset, next_period_start).await? {
                    let (up_token_id, down_token_id) = self.discovery.get_market_tokens(&next_market.condition_id).await?;
                    
                    // Place limit orders
                    let up_order = self.place_limit_order(&up_token_id, "BUY").await?;
                    let down_order = self.place_limit_order(&down_token_id, "BUY").await?;
                    
                    let new_state = PreLimitOrderState {
                        asset: asset.to_string(),
                        condition_id: next_market.condition_id,
                        up_token_id: up_token_id.clone(),
                        down_token_id: down_token_id.clone(),
                        up_order_id: up_order.order_id,
                        down_order_id: down_order.order_id,
                        up_matched: false,
                        down_matched: false,
                        merged: false,
                        expiry: next_period_start + 900,
                        risk_sold: false, // Not used anymore, but kept for compatibility
                        order_placed_at: current_time_et,
                        market_period_start: next_period_start, // Track which market period orders are for
                    };
                    states.insert(asset.to_string(), new_state);
                    
                    // Subscribe to WebSocket for these tokens
                    if let Err(e) = self.ws_manager.subscribe_to_tokens(vec![up_token_id, down_token_id]).await {
                        log::warn!("Failed to subscribe to tokens via WebSocket: {}", e);
                    }
                    
                    return Ok(());
                }
            }
        }

        // 2. Monitor existing state
        if let Some(mut s) = state {
            // Always check order matches (even if one side is already matched, we need to check the other)
            self.check_order_matches(&mut s).await?;

            // Check if we should merge
            if s.up_matched && s.down_matched && !s.merged {
                let profit_per_market = self.config.strategy.shares * 0.10; // $0.10 per share
                
                log::info!("Both orders matched for {}. Merging positions...", asset);
                if self.config.strategy.simulation_mode {
                    log::info!("🎮 SIMULATION: Would merge {} shares for condition {}", 
                        self.config.strategy.shares, s.condition_id);
                    
                    // Update total profit
                    let mut total = self.total_profit.lock().await;
                    *total += profit_per_market;
                    let current_total = *total;
                    drop(total);
                    
                    log::info!("   💰 SIMULATION: Profit locked in: ${:.2} ({} shares × $0.10) | Total Profit: ${:.2}", 
                        profit_per_market, self.config.strategy.shares, current_total);
                    s.merged = true;
                } else {
                    if let Ok(_) = self.api.merge_positions(&s.condition_id, self.config.strategy.shares).await {
                        // Update total profit
                        let mut total = self.total_profit.lock().await;
                        *total += profit_per_market;
                        let current_total = *total;
                        drop(total);
                        
                        log::info!("   💰 Profit locked in: ${:.2} ({} shares × $0.10) | Total Profit: ${:.2}", 
                            profit_per_market, self.config.strategy.shares, current_total);
                        s.merged = true;
                    }
                }
            }

            // 4. Sell unmatched positions after 5 minutes if only one side matched
            let current_time_et = Self::get_current_time_et();
            let time_since_market_start = current_time_et - s.market_period_start;
            let sell_after_seconds = (self.config.strategy.sell_unmatched_after_mins * 60) as i64;
            
            if time_since_market_start >= sell_after_seconds && !s.merged && !s.risk_sold {
                if s.up_matched && !s.down_matched {
                    // Up token matched, Down didn't - sell Up token and cancel Down order
                    log::warn!("{}: 5+ minutes passed, only Up token matched. Selling Up token and canceling Down order", asset);
                    
                    // Get current sell price for Up token
                    let sell_price_result = self.api.get_price(&s.up_token_id, "SELL").await;
                    let purchase_price = self.config.strategy.price_limit; // $0.45
                    
                    if self.config.strategy.simulation_mode {
                        let sell_price = sell_price_result
                            .ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                        
                        // Update total profit (subtract loss)
                        let mut total = self.total_profit.lock().await;
                        *total -= loss;
                        let current_total = *total;
                        drop(total);
                        
                        log::warn!("🎮 SIMULATION: Would sell {} Up token shares at ${:.4} (purchased at ${:.2})", 
                            self.config.strategy.shares, sell_price, purchase_price);
                        if let Some(down_order_id) = &s.down_order_id {
                            log::warn!("🎮 SIMULATION: Would cancel Down order {}", down_order_id);
                        }
                        log::warn!("   💸 SIMULATION: Loss: ${:.2} | Total Profit: ${:.2}", loss, current_total);
                    } else {
                        let sell_price = sell_price_result
                            .ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        // Sell the Up token
                        if let Err(e) = self.api.place_market_order(&s.up_token_id, self.config.strategy.shares, "SELL", None).await {
                            log::error!("Failed to sell Up token for {}: {}", asset, e);
                        } else {
                            // Cancel the Down order
                            if let Some(down_order_id) = &s.down_order_id {
                                if let Err(e) = self.api.cancel_order(down_order_id).await {
                                    log::error!("Failed to cancel Down order for {}: {}", asset, e);
                                } else {
                                    log::info!("✅ Canceled Down order {} for {}", down_order_id, asset);
                                }
                            }
                            
                            let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                            
                            // Update total profit (subtract loss)
                            let mut total = self.total_profit.lock().await;
                            *total -= loss;
                            let current_total = *total;
                            drop(total);
                            
                            log::warn!("   💸 Sold {} Up token shares at ${:.2} (purchased at ${:.2})", 
                                self.config.strategy.shares, sell_price, purchase_price);
                            log::warn!("   💸 Loss: ${:.2} | Total Profit: ${:.2}", loss, current_total);
                        }
                    }
                    s.risk_sold = true; // Mark as sold to prevent duplicate actions
                    s.merged = true; // Mark as merged to prevent merge attempts after selling
                } else if s.down_matched && !s.up_matched {
                    // Down token matched, Up didn't - sell Down token and cancel Up order
                    log::warn!("{}: 5+ minutes passed, only Down token matched. Selling Down token and canceling Up order", asset);
                    
                    // Get current sell price for Down token
                    let sell_price_result = self.api.get_price(&s.down_token_id, "SELL").await;
                    let purchase_price = self.config.strategy.price_limit; // $0.45
                    
                    if self.config.strategy.simulation_mode {
                        let sell_price = sell_price_result
                            .ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                        
                        // Update total profit (subtract loss)
                        let mut total = self.total_profit.lock().await;
                        *total -= loss;
                        let current_total = *total;
                        drop(total);
                        
                        log::warn!("🎮 SIMULATION: Would sell {} Down token shares at ${:.4} (purchased at ${:.2})", 
                            self.config.strategy.shares, sell_price, purchase_price);
                        if let Some(up_order_id) = &s.up_order_id {
                            log::warn!("🎮 SIMULATION: Would cancel Up order {}", up_order_id);
                        }
                        log::warn!("   💸 SIMULATION: Loss: ${:.2} | Total Profit: ${:.2}", loss, current_total);
                    } else {
                        let sell_price = sell_price_result
                            .ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        // Sell the Down token
                        if let Err(e) = self.api.place_market_order(&s.down_token_id, self.config.strategy.shares, "SELL", None).await {
                            log::error!("Failed to sell Down token for {}: {}", asset, e);
                        } else {
                            // Cancel the Up order
                            if let Some(up_order_id) = &s.up_order_id {
                                if let Err(e) = self.api.cancel_order(up_order_id).await {
                                    log::error!("Failed to cancel Up order for {}: {}", asset, e);
                                } else {
                                    log::info!("✅ Canceled Up order {} for {}", up_order_id, asset);
                                }
                            }
                            
                            let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                            
                            // Update total profit (subtract loss)
                            let mut total = self.total_profit.lock().await;
                            *total -= loss;
                            let current_total = *total;
                            drop(total);
                            
                            log::warn!("   💸 Sold {} Down token shares at ${:.2} (purchased at ${:.2})", 
                                self.config.strategy.shares, sell_price, purchase_price);
                            log::warn!("   💸 Loss: ${:.2} | Total Profit: ${:.2}", loss, current_total);
                        }
                    }
                    s.risk_sold = true; // Mark as sold to prevent duplicate actions
                    s.merged = true; // Mark as merged to prevent merge attempts after selling
                    s.merged = true; // Mark as merged to prevent merge attempts after selling
                }
            }

            // Cleanup old states
            let current_time_et = Self::get_current_time_et();
            if current_time_et > s.expiry {
                log::info!("Market expired for {}. Clearing state.", asset);
                states.remove(asset);
            } else {
                states.insert(asset.to_string(), s);
            }
        }

        Ok(())
    }

    async fn discover_next_market(&self, asset_name: &str, next_timestamp: i64) -> Result<Option<Market>> {
        let asset_slug = match asset_name {
            "BTC" => "btc",
            "ETH" => "eth",
            "SOL" => "sol",
            "XRP" => "xrp",
            _ => return Ok(None),
        };
        
        let slug = format!("{}-updown-15m-{}", asset_slug, next_timestamp);
        match self.api.get_market_by_slug(&slug).await {
            Ok(m) => {
                if m.active && !m.closed {
                    Ok(Some(m))
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                log::debug!("Failed to find market with slug {}: {}", slug, e);
                Ok(None)
            }
        }
    }

    async fn place_limit_order(&self, token_id: &str, side: &str) -> Result<OrderResponse> {
        if self.config.strategy.simulation_mode {
            log::info!("🎮 SIMULATION: Would place {} order for token {}: {} shares @ ${:.2}", 
                side, token_id, self.config.strategy.shares, self.config.strategy.price_limit);
            
            // Generate a fake order ID for simulation
            let fake_order_id = format!("SIM-{}-{}", side, chrono::Utc::now().timestamp());
            
            Ok(OrderResponse {
                order_id: Some(fake_order_id),
                status: "SIMULATED".to_string(),
                message: Some("Order simulated (not placed)".to_string()),
            })
        } else {
            let order = OrderRequest {
                token_id: token_id.to_string(),
                side: side.to_string(),
                size: self.config.strategy.shares.to_string(),
                price: self.config.strategy.price_limit.to_string(),
                order_type: "LIMIT".to_string(),
            };
            self.api.place_order(&order).await
        }
    }

    async fn check_order_matches(&self, state: &mut PreLimitOrderState) -> Result<()> {
        let current_time_et = Self::get_current_time_et();
        
        // IMPORTANT: Only check matches if the market where orders were placed has actually started
        // Market starts at market_period_start. Orders can't match before the market is active.
        // This check applies to BOTH simulation and production modes.
        if current_time_et < state.market_period_start {
            // Market hasn't started yet, can't match orders - return early
            log::debug!("Market {} for {} hasn't started yet (current: {}, start: {})", 
                state.market_period_start, state.asset, current_time_et, state.market_period_start);
            return Ok(());
        }
        
        // Both simulation and production modes check prices to determine if orders matched
        // We placed BUY orders at $0.45, so if price is <= $0.45, our order should match
        // Try WebSocket first, fall back to REST API
        
        // Get Up token price (try WebSocket first, fallback to REST API)
        // For BUY orders, we need ASK price (what sellers are asking)
        // WebSocket returns ASK when side="BUY", but REST API returns BID
        // So for REST fallback, use "SELL" to get ASK price
        let up_price_ws = self.ws_manager.get_price(&state.up_token_id, "BUY").await;
        let up_price_result = if let Some(price) = up_price_ws {
            Ok(price)
        } else {
            // REST API: side="SELL" returns ASK price (what sellers are asking)
            self.api.get_price(&state.up_token_id, "SELL").await
        };
        
        // Get Down token price (try WebSocket first, fallback to REST API)
        let down_price_ws = self.ws_manager.get_price(&state.down_token_id, "BUY").await;
        let down_price_result = if let Some(price) = down_price_ws {
            Ok(price)
        } else {
            // REST API: side="SELL" returns ASK price (what sellers are asking)
            self.api.get_price(&state.down_token_id, "SELL").await
        };
        
        if let Ok(up_price) = up_price_result {
            let up_price_f64: f64 = up_price.to_string().parse().unwrap_or(0.0);
            // If price is at or below our limit price, order matched
            // Use a small epsilon for floating point comparison to handle precision issues
            let price_limit = self.config.strategy.price_limit;
            if (up_price_f64 <= price_limit || (up_price_f64 - price_limit).abs() < 0.001) && !state.up_matched {
                if self.config.strategy.simulation_mode {
                    log::info!("🎮 SIMULATION: Up order matched for {} (price hit ${:.4} <= ${:.2})", 
                        state.asset, up_price_f64, price_limit);
                } else {
                    log::info!("✅ Up order matched for {} (price hit ${:.4} <= ${:.2})", 
                        state.asset, up_price_f64, price_limit);
                }
                state.up_matched = true;
            }
        }
        
        if let Ok(down_price) = down_price_result {
            let down_price_f64: f64 = down_price.to_string().parse().unwrap_or(0.0);
            // If price is at or below our limit price, order matched
            // Use a small epsilon for floating point comparison to handle precision issues
            let price_limit = self.config.strategy.price_limit;
            let price_matches = down_price_f64 <= price_limit || (down_price_f64 - price_limit).abs() < 0.001;
            
            log::debug!("Checking Down order for {}: price=${:.2}, limit=${:.2}, matches={}, already_matched={}", 
                state.asset, down_price_f64, price_limit, price_matches, state.down_matched);
            
            if price_matches && !state.down_matched {
                if self.config.strategy.simulation_mode {
                    log::info!("🎮 SIMULATION: Down order matched for {} (price hit ${:.2} <= ${:.2})", 
                        state.asset, down_price_f64, price_limit);
                } else {
                    log::info!("✅ Down order matched for {} (price hit ${:.2} <= ${:.2})", 
                        state.asset, down_price_f64, price_limit);
                }
                state.down_matched = true;
            }
        } else {
            log::debug!("Failed to get Down price for {}: {:?}", state.asset, down_price_result);
        }
        Ok(())
    }

    async fn display_market_status(&self) -> Result<()> {
        let assets = vec!["BTC", "ETH", "SOL", "XRP"];
        let current_time_et = Self::get_current_time_et();
        
        let total_profit = {
            let total = self.total_profit.lock().await;
            *total
        };
        
        log::info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        log::info!("📊 Market Status Update | 💰 Total Profit: ${:.2}", total_profit);
        log::info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        let mut states = self.states.lock().await;
        let mut states_to_check: Vec<String> = Vec::new();
        
        for asset in &assets {
            let asset_slug = match *asset {
                "BTC" => "btc",
                "ETH" => "eth",
                "SOL" => "sol",
                "XRP" => "xrp",
                _ => continue,
            };
            
            // Check if we have orders placed (state exists)
            if let Some(state) = states.get_mut(*asset) {
                // Display the market where orders were placed (not current market)
                let market_period = state.market_period_start;
                let slug = format!("{}-updown-15m-{}", asset_slug, market_period);
                
                match self.api.get_market_by_slug(&slug).await {
                    Ok(market) => {
                        if market.active && !market.closed {
                            // Get prices for the market where orders were placed (try WebSocket first, fallback to REST)
                            // For BUY orders, we need ASK price (what sellers are asking)
                            let up_price_ws = self.ws_manager.get_price(&state.up_token_id, "BUY").await;
                            let up_price_result = if let Some(price) = up_price_ws {
                                Ok(price)
                            } else {
                                // REST API: side="SELL" returns ASK price (what sellers are asking)
                                self.api.get_price(&state.up_token_id, "SELL").await
                            };
                            
                            let down_price_ws = self.ws_manager.get_price(&state.down_token_id, "BUY").await;
                            let down_price_result = if let Some(price) = down_price_ws {
                                Ok(price)
                            } else {
                                // REST API: side="SELL" returns ASK price (what sellers are asking)
                                self.api.get_price(&state.down_token_id, "SELL").await
                            };
                            
                            // Calculate remaining time for the market where orders were placed
                            let market_end = market_period + 900;
                            let time_remaining = market_end - current_time_et;
                            let minutes = if time_remaining > 0 { time_remaining / 60 } else { 0 };
                            let seconds = if time_remaining > 0 { time_remaining % 60 } else { 0 };
                            
                            // Format prices (2 decimals)
                            let up_price_str = match up_price_result {
                                Ok(p) => format!("${:.2}", p),
                                Err(_) => "N/A".to_string(),
                            };
                            let down_price_str = match down_price_result {
                                Ok(p) => format!("${:.2}", p),
                                Err(_) => "N/A".to_string(),
                            };
                            
                            // Orders status: Only show checkmark based on state (once matched, stays matched)
                            // Also check current prices to trigger state update if needed
                            let price_limit = self.config.strategy.price_limit;
                            let up_price_matched = up_price_result.as_ref()
                                .ok()
                                .and_then(|p| p.to_string().parse::<f64>().ok())
                                .map(|p| {
                                    let price_f64 = p;
                                    price_f64 <= price_limit || (price_f64 - price_limit).abs() < 0.001
                                })
                                .unwrap_or(false);
                            let down_price_matched = down_price_result.as_ref()
                                .ok()
                                .and_then(|p| p.to_string().parse::<f64>().ok())
                                .map(|p| {
                                    let price_f64 = p;
                                    price_f64 <= price_limit || (price_f64 - price_limit).abs() < 0.001
                                })
                                .unwrap_or(false);
                            
                            // If prices hit limit but state not updated, update state immediately
                            // This ensures the checkmark appears right away
                            if up_price_matched && !state.up_matched {
                                state.up_matched = true;
                                states_to_check.push(asset.to_string());
                                log::debug!("Display: Up order matched for {} (price hit limit)", asset);
                            }
                            if down_price_matched && !state.down_matched {
                                state.down_matched = true;
                                states_to_check.push(asset.to_string());
                                log::debug!("Display: Down order matched for {} (price hit limit)", asset);
                            }
                            
                            // Display: Only use state flags (once matched, always show ✓)
                            // Don't check current prices for display - state persists the match status
                            let order_status = format!("Up:{} Down:{}", 
                                if state.up_matched { "✓" } else { "⏳" },
                                if state.down_matched { "✓" } else { "⏳" });
                            
                            log::info!("{} | Up: {} | Down: {} | Time: {}m {}s | Orders: {} | Market: {}", 
                                asset, up_price_str, down_price_str, minutes, seconds, order_status, market_period);
                        } else {
                            log::info!("{} | Market {} inactive/closed | Orders: Up:{} Down:{}", 
                                asset, market_period,
                                if state.up_matched { "✓" } else { "⏳" },
                                if state.down_matched { "✓" } else { "⏳" });
                        }
                    }
                    Err(_) => {
                        log::info!("{} | Market {} not found | Orders: Up:{} Down:{}", 
                            asset, market_period,
                            if state.up_matched { "✓" } else { "⏳" },
                            if state.down_matched { "✓" } else { "⏳" });
                    }
                }
            } else {
                // No orders placed yet - show current market
                let current_period_et = Self::get_current_15m_period_et();
                let slug = format!("{}-updown-15m-{}", asset_slug, current_period_et);
                
                match self.api.get_market_by_slug(&slug).await {
                    Ok(market) => {
                        if market.active && !market.closed {
                            match self.api.get_market(&market.condition_id).await {
                                Ok(_) => {
                                    match self.discovery.get_market_tokens(&market.condition_id).await {
                                        Ok((up_token_id, down_token_id)) => {
                                            // Get prices (try WebSocket first, fallback to REST API)
                                            let (up_price_result, down_price_result) = tokio::join!(
                                                async {
                                                    if let Some(price) = self.ws_manager.get_price(&up_token_id, "BUY").await {
                                                        Ok(price)
                                                    } else {
                                                        self.api.get_price(&up_token_id, "SELL").await
                                                    }
                                                },
                                                async {
                                                    if let Some(price) = self.ws_manager.get_price(&down_token_id, "BUY").await {
                                                        Ok(price)
                                                    } else {
                                                        self.api.get_price(&down_token_id, "SELL").await
                                                    }
                                                }
                                            );
                                            
                                            let market_end = current_period_et + 900;
                                            let time_remaining = market_end - current_time_et;
                                            let minutes = if time_remaining > 0 { time_remaining / 60 } else { 0 };
                                            let seconds = if time_remaining > 0 { time_remaining % 60 } else { 0 };
                                            
                                            // Format prices (2 decimals)
                                            let up_price_str = match up_price_result {
                                                Ok(p) => format!("${:.2}", p),
                                                Err(_) => "N/A".to_string(),
                                            };
                                            let down_price_str = match down_price_result {
                                                Ok(p) => format!("${:.2}", p),
                                                Err(_) => "N/A".to_string(),
                                            };
                                            
                                            log::info!("{} | Up: {} | Down: {} | Time: {}m {}s | Orders: No orders | Market: {}", 
                                                asset, up_price_str, down_price_str, minutes, seconds, current_period_et);
                                        }
                                        Err(_) => {
                                            log::info!("{} | Current market found but failed to get tokens", asset);
                                        }
                                    }
                                }
                                Err(_) => {
                                    log::info!("{} | Current market found but failed to get details", asset);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        log::info!("{} | Current market not found", asset);
                    }
                }
            }
        }
        
        // States are already updated in the loop above (get_mut modifies in place)
        drop(states);
        log::info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        
        // Also call check_order_matches for assets where prices hit the limit
        // This ensures proper logging and any additional logic in check_order_matches runs
        // This ensures state is updated immediately when prices hit the limit
        for asset in states_to_check {
            let mut states = self.states.lock().await;
            if let Some(mut state) = states.get_mut(&asset) {
                // Check and update matches based on current prices
                // Note: get_mut gives us a mutable reference, so changes are already in the HashMap
                let before_up = state.up_matched;
                let before_down = state.down_matched;
                
                if let Err(e) = self.check_order_matches(&mut state).await {
                    log::debug!("Error checking order matches for {}: {}", asset, e);
                }
                
                // Log if state was updated
                if state.up_matched != before_up || state.down_matched != before_down {
                    log::debug!("State updated for {}: up_matched={}->{}, down_matched={}->{}", 
                        asset, before_up, state.up_matched, before_down, state.down_matched);
                }
            }
        }
        
        Ok(())
    }
}

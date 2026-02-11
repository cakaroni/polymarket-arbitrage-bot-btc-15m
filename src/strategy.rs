use crate::api::PolymarketApi;
use crate::config::Config;
use crate::discovery::{MarketDiscovery, ASSET_TO_SLUG};
use crate::models::*;
use crate::signals::{self, MarketSignal};
use anyhow::Result;
use chrono::Utc;
use chrono_tz::America::New_York;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use log::{warn, info, error, debug};

pub struct PreLimitStrategy {
    api: Arc<PolymarketApi>,
    config: Config,
    discovery: MarketDiscovery,
    states: Arc<Mutex<HashMap<String, PreLimitOrderState>>>,
    last_status_display: Arc<Mutex<std::time::Instant>>,
    total_profit: Arc<Mutex<f64>>,
    trades: Arc<Mutex<HashMap<String, CycleTrade>>>,
    closure_checked: Arc<Mutex<HashMap<String, bool>>>,
    period_profit: Arc<Mutex<f64>>,
}

#[derive(Debug, Clone)]
struct CycleTrade {
    condition_id: String,
    period_timestamp: u64,
    market_duration_secs: u64,
    up_token_id: Option<String>,
    down_token_id: Option<String>,
    up_shares: f64,
    down_shares: f64,
    up_avg_price: f64,
    down_avg_price: f64,
}

impl PreLimitStrategy {
    pub fn new(api: Arc<PolymarketApi>, config: Config) -> Self {
        let discovery = MarketDiscovery::new(api.clone());
        Self {
            api,
            config,
            discovery,
            states: Arc::new(Mutex::new(HashMap::new())),
            last_status_display: Arc::new(Mutex::new(std::time::Instant::now())),
            total_profit: Arc::new(Mutex::new(0.0)),
            trades: Arc::new(Mutex::new(HashMap::new())),
            closure_checked: Arc::new(Mutex::new(HashMap::new())),
            period_profit: Arc::new(Mutex::new(0.0)),
        }
    }

    pub async fn get_total_profit(&self) -> f64 {
        *self.total_profit.lock().await
    }

    pub async fn get_period_profit(&self) -> f64 {
        *self.period_profit.lock().await
    }

    pub async fn run(&self) -> Result<()> {
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
        let current_period_et = Self::get_current_1h_period_et();
        
        for asset in assets {
            self.process_asset(asset, current_period_et).await?;
        }
        Ok(())
    }
    
    /// Calculate the current running 1-hour period timestamp in ET timezone
    fn get_current_1h_period_et() -> i64 {
        MarketDiscovery::current_1h_period_start_et()
    }
    
    fn get_current_time_et() -> i64 {
        let now_utc = Utc::now();
        let now_et = now_utc.with_timezone(&New_York);
        now_et.timestamp()
    }

    async fn process_asset(&self, asset: &str, current_period_et: i64) -> Result<()> {
        let mut states = self.states.lock().await;
        let state = states.get(asset).cloned();
        
        let current_time_et = Self::get_current_time_et();
        let next_period_start = current_period_et + 3600;
        let time_until_next = next_period_start - current_time_et;

        let needs_danger_handling = state.as_ref().map_or(false, |s| {
            !s.merged && !s.risk_sold &&
            ((s.up_matched && !s.down_matched) || (s.down_matched && !s.up_matched))
        });

        if time_until_next <= (self.config.strategy.place_order_before_mins * 60) as i64 {
            let is_next_market_prepared = state.as_ref().map_or(false, |s| s.expiry == next_period_start + 3600);
            
            if !is_next_market_prepared && !needs_danger_handling {
                // Signal check: evaluate current market before placing pre-orders for next
                let signal = self.get_place_signal(asset, current_period_et).await;
                if signal != MarketSignal::Good {
                    if signal == MarketSignal::Bad {
                        log::info!("{} | Bad signal for current market — skipping pre-orders for next hour", asset);
                    }
                } else if let Some(next_market) = self.discover_next_market(asset, next_period_start).await? {
                    log::info!("Preparing orders for next {} market (starts in {}s)", asset, time_until_next);
                    let (up_token_id, down_token_id) = self.discovery.get_market_tokens(&next_market.condition_id).await?;

                    let price_limit = self.config.strategy.price_limit;
                    let up_order = self.place_limit_order(&up_token_id, "BUY", price_limit).await?;
                    let down_order = self.place_limit_order(&down_token_id, "BUY", price_limit).await?;
                    
                    let new_state = PreLimitOrderState {
                        asset: asset.to_string(),
                        condition_id: next_market.condition_id,
                        up_token_id: up_token_id.clone(),
                        down_token_id: down_token_id.clone(),
                        up_order_id: up_order.order_id,
                        down_order_id: down_order.order_id,
                        up_order_price: price_limit,
                        down_order_price: price_limit,
                        up_matched: false,
                        down_matched: false,
                        merged: false,
                        expiry: next_period_start + 3600,
                        risk_sold: false,
                        order_placed_at: current_time_et,
                        market_period_start: next_period_start,
                        one_side_matched_at: None,
                    };
                    states.insert(asset.to_string(), new_state);
                    
                    return Ok(());
                } else {
                    log::debug!("Could not find next {} market - slug may be incorrect or market not yet available", asset);
                }
            }
        }

        if let Some(mut s) = state {
            self.check_order_matches(&mut s).await?;

            if s.up_matched && s.down_matched && !s.merged {
                let threshold = self.config.strategy.sell_opposite_above;
                let (up_price, down_price) = (
                    self.api.get_price(&s.up_token_id, "SELL").await.ok()
                        .and_then(|p| p.to_string().parse::<f64>().ok()).unwrap_or(0.0),
                    self.api.get_price(&s.down_token_id, "SELL").await.ok()
                        .and_then(|p| p.to_string().parse::<f64>().ok()).unwrap_or(0.0),
                );

                // Calculate time remaining in the current market period
                let current_time_et = Self::get_current_time_et();
                let market_end_time = s.market_period_start + 3600; // 1 hour market
                let time_remaining_seconds = market_end_time - current_time_et;
                let time_remaining_mins = time_remaining_seconds / 60;
                let required_time_remaining_mins = self.config.strategy.sell_opposite_time_remaining as i64;

                let sell_opposite = if up_price >= threshold {
                    Some(("Up", "Down", &s.down_token_id, s.down_order_price))
                } else if down_price >= threshold {
                    Some(("Down", "Up", &s.up_token_id, s.up_order_price))
                } else {
                    None
                };

                // Only sell if BOTH conditions are met: price threshold AND time remaining is low enough
                if let Some((winner, loser, token_to_sell, purchase_price)) = sell_opposite {
                    if time_remaining_mins <= required_time_remaining_mins {
                        log::info!("{}: Both filled, {} price ${:.2} >= {:.2} AND {}min remaining <= {}min — selling {} to reduce loss", 
                            asset, winner, if winner == "Up" { up_price } else { down_price }, threshold, 
                            time_remaining_mins, required_time_remaining_mins, loser);
                        let sell_price_result = self.api.get_price(token_to_sell, "SELL").await;
                        let sell_price = sell_price_result.ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok()).unwrap_or(0.0);
                        if self.config.strategy.simulation_mode {
                            let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                            let mut total = self.total_profit.lock().await;
                            *total -= loss;
                            let current_total = *total;
                            drop(total);
                            log::info!("🎮 SIMULATION: Would sell {} {} shares at ${:.4} (purchased at ${:.2})", 
                                self.config.strategy.shares, loser, sell_price, purchase_price);
                            log::info!("   Holding {} to expiry (pays $1). Loss on {}: ${:.2} | Total Profit: ${:.2}", 
                                winner, loser, loss, current_total);
                        } else {
                            if let Err(e) = self.api.place_market_order(&token_to_sell, self.config.strategy.shares, "SELL", None).await {
                                log::error!("Failed to sell {} token for {}: {}", loser, asset, e);
                            } else {
                                let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                                let mut total = self.total_profit.lock().await;
                                *total -= loss;
                                let current_total = *total;
                                drop(total);
                                log::info!("   Sold {} {} shares at ${:.2}. Holding {} to expiry (pays $1). Loss: ${:.2} | Total Profit: ${:.2}", 
                                    self.config.strategy.shares, loser, sell_price, winner, loss, current_total);
                            }
                        }
                        s.merged = true;
                        // Register for redemption (production only): holding winner, check_market_closure will redeem when market resolves
                        if !self.config.strategy.simulation_mode {
                            let trade = Self::cycle_trade_holding_winner(&s, winner, self.config.strategy.shares);
                            let mut t = self.trades.lock().await;
                            t.insert(s.condition_id.clone(), trade);
                            log::info!("   Registered position for redemption when market resolves (condition {})", &s.condition_id[..s.condition_id.len().min(20)]);
                        }
                    } else {
                        log::debug!("{}: {} price ${:.2} >= {:.2}, but {}min remaining > {}min threshold — holding both positions", 
                            asset, winner, if winner == "Up" { up_price } else { down_price }, threshold,
                            time_remaining_mins, required_time_remaining_mins);
                    }
                }
                // When both filled but neither side >= sell_opposite_above: do nothing.
                // Hold both until one side hits threshold (re-check next tick) or expiry (redeem).
            }

            let current_time_et = Self::get_current_time_et();
            let time_since_market_start = current_time_et - s.market_period_start;
            let sell_after_seconds = (self.config.strategy.sell_unmatched_after_mins * 60) as i64;

            // Track when we first had only one side matched (for sell_after_danger_time_passed)
            let only_one_matched = (s.up_matched && !s.down_matched) || (s.down_matched && !s.up_matched);
            if only_one_matched && s.one_side_matched_at.is_none() {
                s.one_side_matched_at = Some(current_time_et);
            }

            // One-side risk management: optional early sell (in addition to sell_unmatched_after_mins timeout)
            // "price" = sell when matched token <= danger_price; "time" = sell after danger_time_passed mins
            let mode = match self.config.strategy.signal.one_side_buy_risk_management.to_lowercase().as_str() {
                "price" | "sell_at_danger_price" => "price",
                "time" | "sell_after_danger_time_passed" => "time",
                _ => "none",
            };
            let mut should_sell_early = if !only_one_matched {
                false
            } else if mode == "price" {
                if s.up_matched && !s.down_matched {
                    self.api.get_price(&s.up_token_id, "SELL").await
                        .ok()
                        .and_then(|p| p.to_string().parse::<f64>().ok())
                        .map(|p| signals::is_danger_signal(&self.config.strategy.signal, p))
                        .unwrap_or(false)
                } else {
                    self.api.get_price(&s.down_token_id, "SELL").await
                        .ok()
                        .and_then(|p| p.to_string().parse::<f64>().ok())
                        .map(|p| signals::is_danger_signal(&self.config.strategy.signal, p))
                        .unwrap_or(false)
                }
            } else if mode == "sell_after_danger_time_passed" {
                let danger_mins = self.config.strategy.signal.danger_time_passed as i64;
                s.one_side_matched_at.map_or(false, |t| current_time_et - t >= danger_mins * 60)
            } else {
                false
            };

            // Production only: when danger would trigger, verify both orders via API first.
            // If both filled, don't sell — update state and let "both matched" logic handle next tick.
            if !self.config.strategy.simulation_mode && should_sell_early {
                if let (Some(up_id), Some(down_id)) = (&s.up_order_id, &s.down_order_id) {
                    match self.api.are_both_orders_filled(up_id, down_id).await {
                        Ok((true, true)) => {
                            log::info!("{}: Danger signal but both orders filled (verified via API) — skipping sell", asset);
                            s.up_matched = true;
                            s.down_matched = true;
                            should_sell_early = false;
                        }
                        Ok(_) => { /* one or both not filled, proceed with sell */ }
                        Err(e) => {
                            log::warn!("{}: Failed to verify order status: {} — proceeding with danger sell", asset, e);
                        }
                    }
                }
            }

            let should_sell = !s.merged && !s.risk_sold && (
                should_sell_early ||
                time_since_market_start >= sell_after_seconds
            );

            if should_sell {
                let reason = if should_sell_early {
                    if mode == "time" {
                        format!("Danger time passed ({}min since match)", self.config.strategy.signal.danger_time_passed)
                    } else {
                        "Danger signal (price collapsed)".to_string()
                    }
                } else {
                    "Timeout reached".to_string()
                };
                if s.up_matched && !s.down_matched {
                    log::warn!("{}: {} — only Up token matched. Selling Up token and canceling Down order", asset, reason.as_str());
                    
                    let sell_price_result = self.api.get_price(&s.up_token_id, "SELL").await;
                    let purchase_price = s.up_order_price;
                    
                    if self.config.strategy.simulation_mode {
                        let sell_price = sell_price_result
                            .ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                        
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
                            if let Some(down_order_id) = &s.down_order_id {
                                if let Err(e) = self.api.cancel_order(down_order_id).await {
                                    log::error!("Failed to cancel Down order for {}: {}", asset, e);
                                } else {
                                    log::info!("✅ Canceled Down order {} for {}", down_order_id, asset);
                                }
                            }
                            
                            let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                            
                            let mut total = self.total_profit.lock().await;
                            *total -= loss;
                            let current_total = *total;
                            drop(total);
                            
                            log::warn!("   💸 Sold {} Up token shares at ${:.2} (purchased at ${:.2})", 
                                self.config.strategy.shares, sell_price, purchase_price);
                            log::warn!("   💸 Loss: ${:.2} | Total Profit: ${:.2}", loss, current_total);
                        }
                    }
                    s.risk_sold = true;
                    s.merged = true;
                } else if s.down_matched && !s.up_matched {
                    log::warn!("{}: {} — only Down token matched. Selling Down token and canceling Up order", asset, reason.as_str());
                    
                    // Get current sell price for Down token
                    let sell_price_result = self.api.get_price(&s.down_token_id, "SELL").await;
                    let purchase_price = s.down_order_price;
                    
                    if self.config.strategy.simulation_mode {
                        let sell_price = sell_price_result
                            .ok()
                            .and_then(|p| p.to_string().parse::<f64>().ok())
                            .unwrap_or(0.0);
                        
                        let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                        
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
                        
                        if let Err(e) = self.api.place_market_order(&s.down_token_id, self.config.strategy.shares, "SELL", None).await {
                            log::error!("Failed to sell Down token for {}: {}", asset, e);
                        } else {
                            if let Some(up_order_id) = &s.up_order_id {
                                if let Err(e) = self.api.cancel_order(up_order_id).await {
                                    log::error!("Failed to cancel Up order for {}: {}", asset, e);
                                } else {
                                    log::info!("✅ Canceled Up order {} for {}", up_order_id, asset);
                                }
                            }
                            
                            let loss = (purchase_price - sell_price) * self.config.strategy.shares;
                            
                            let mut total = self.total_profit.lock().await;
                            *total -= loss;
                            let current_total = *total;
                            drop(total);
                            
                            log::warn!("   💸 Sold {} Down token shares at ${:.2} (purchased at ${:.2})", 
                                self.config.strategy.shares, sell_price, purchase_price);
                            log::warn!("   💸 Loss: ${:.2} | Total Profit: ${:.2}", loss, current_total);
                        }
                    }
                    s.risk_sold = true;
                    s.merged = true;
                }
            }

            let current_time_et = Self::get_current_time_et();
            if current_time_et > s.expiry {
                // Register for redemption (production only) if we held both until expiry (sold opposite already registered)
                if !self.config.strategy.simulation_mode && s.up_matched && s.down_matched && !s.risk_sold && !s.merged {
                    let trade = Self::cycle_trade_holding_both(&s, self.config.strategy.shares);
                    let mut t = self.trades.lock().await;
                    t.insert(s.condition_id.clone(), trade);
                    log::info!("   Registered position for redemption when market resolves (condition {})", &s.condition_id[..s.condition_id.len().min(20)]);
                }
                log::info!("Market expired for {}. Clearing state.", asset);
                states.remove(asset);
            } else {
                states.insert(asset.to_string(), s);
            }
        } else if time_until_next > (self.config.strategy.place_order_before_mins * 60) as i64
            && self.config.strategy.signal.mid_market_enabled
        {
            // Don't place mid-market orders if too little time remains — we'd hit timeout and sell at a loss.
            let time_remaining_in_current_market = (current_period_et + 3600) - current_time_et;
            let min_remaining_to_place = (self.config.strategy.sell_unmatched_after_mins * 60) as i64;
            if time_remaining_in_current_market < min_remaining_to_place {
                log::debug!("{} | Skipping mid-market orders: only {}s left (need {}s to avoid immediate timeout)",
                    asset, time_remaining_in_current_market, min_remaining_to_place);
            } else {
            let signal = self.get_place_signal(asset, current_period_et).await;
            if signal == MarketSignal::Good {
                if let Some(current_market) = self.discover_next_market(asset, current_period_et).await? {
                    let Some((up_price, down_price, _)) = self.get_market_snapshot(asset, current_period_et).await else {
                        return Ok(());
                    };
                    let (up_order_price, down_order_price) = if up_price <= down_price {
                        (Self::round_price(up_price), Self::round_price(0.98 - up_price))
                    } else {
                        (Self::round_price(0.98 - down_price), Self::round_price(down_price))
                    };
                    log::info!("{} | Good signal — placing mid-market orders: Up @ ${:.2}, Down @ ${:.2} (current Up ${:.2}, Down ${:.2})", 
                        asset, up_order_price, down_order_price, up_price, down_price);
                    let (up_token_id, down_token_id) = self.discovery.get_market_tokens(&current_market.condition_id).await?;
                    let up_order = self.place_limit_order(&up_token_id, "BUY", up_order_price).await?;
                    let down_order = self.place_limit_order(&down_token_id, "BUY", down_order_price).await?;
                    let new_state = PreLimitOrderState {
                        asset: asset.to_string(),
                        condition_id: current_market.condition_id,
                        up_token_id: up_token_id.clone(),
                        down_token_id: down_token_id.clone(),
                        up_order_id: up_order.order_id,
                        down_order_id: down_order.order_id,
                        up_order_price,
                        down_order_price,
                        up_matched: false,
                        down_matched: false,
                        merged: false,
                        expiry: current_period_et + 3600,
                        risk_sold: false,
                        order_placed_at: current_time_et,
                        market_period_start: current_period_et,
                        one_side_matched_at: None,
                    };
                    states.insert(asset.to_string(), new_state);
                    return Ok(());
                }
            }
            }
        }

        Ok(())
    }

    async fn get_market_snapshot(&self, asset: &str, period_start: i64) -> Option<(f64, f64, i64)> {
        let asset_slug = ASSET_TO_SLUG
            .iter()
            .find(|(name, _)| *name == asset)
            .map(|(_, slug)| *slug)
            .unwrap_or("bitcoin");
        let slug = MarketDiscovery::build_1h_slug(asset_slug, period_start);
        let market = self.api.get_market_by_slug(&slug).await.ok()?;
        if !market.active || market.closed {
            return None;
        }
        let (up_token_id, down_token_id) = self.discovery.get_market_tokens(&market.condition_id).await.ok()?;
        let (up_res, down_res) = tokio::join!(
            self.api.get_price(&up_token_id, "SELL"),
            self.api.get_price(&down_token_id, "SELL")
        );
        let up_price = up_res.ok()?.to_string().parse::<f64>().ok()?;
        let down_price = down_res.ok()?.to_string().parse::<f64>().ok()?;
        let current_time_et = Self::get_current_time_et();
        let market_end = period_start + 3600;
        let time_remaining = market_end - current_time_et;
        Some((up_price, down_price, time_remaining.max(0)))
    }

    async fn get_place_signal(&self, asset: &str, period_start: i64) -> MarketSignal {
        let Some((up_price, down_price, time_remaining)) = self.get_market_snapshot(asset, period_start).await else {
            return MarketSignal::Unknown;
        };
        signals::evaluate_place_signal(
            &self.config.strategy.signal,
            up_price,
            down_price,
            time_remaining,
        )
    }

    async fn discover_next_market(&self, asset_name: &str, next_timestamp: i64) -> Result<Option<Market>> {
        let asset_slug = ASSET_TO_SLUG
            .iter()
            .find(|(name, _)| *name == asset_name)
            .map(|(_, slug)| *slug)
            .unwrap_or("bitcoin");
        
        let slug = MarketDiscovery::build_1h_slug(asset_slug, next_timestamp);
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

    pub async fn check_market_closure(&self) -> Result<()> {
        let trades: Vec<(String, CycleTrade)> = {
            let t = self.trades.lock().await;
            t.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        if trades.is_empty() {
            return Ok(());
        }
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        for (market_key, trade) in trades {
            let market_end = trade.period_timestamp + trade.market_duration_secs;
            if current_time < market_end {
                continue;
            }

            let checked = self.closure_checked.lock().await;
            if checked.get(&trade.condition_id).copied().unwrap_or(false) {
                drop(checked);
                continue;
            }
            drop(checked);

            let market = match self.api.get_market(&trade.condition_id).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to fetch market {}: {}", &trade.condition_id[..16], e);
                    continue;
                }
            };
            if !market.closed {
                continue;
            }

            let up_wins = trade
                .up_token_id
                .as_ref()
                .map(|id| market.tokens.iter().any(|t| t.token_id == *id && t.winner))
                .unwrap_or(false);
            let down_wins = trade
                .down_token_id
                .as_ref()
                .map(|id| market.tokens.iter().any(|t| t.token_id == *id && t.winner))
                .unwrap_or(false);

            let total_cost = (trade.up_shares * trade.up_avg_price) + (trade.down_shares * trade.down_avg_price);
            let payout = if up_wins {
                trade.up_shares * 1.0
            } else if down_wins {
                trade.down_shares * 1.0
            } else {
                0.0
            };
            let pnl = payout - total_cost;

            let winner = if up_wins { "Up" } else if down_wins { "Down" } else { "Unknown" };
            eprintln!("=== Market resolved ===");
            eprintln!(
                "Market closed | condition {} | Winner: {} | Up {:.2} @ {:.4} | Down {:.2} @ {:.4} | Cost ${:.2} | Payout ${:.2} | Actual PnL ${:.2}",
                &trade.condition_id[..16],
                winner,
                trade.up_shares,
                trade.up_avg_price,
                trade.down_shares,
                trade.down_avg_price,
                total_cost,
                payout,
                pnl
            );

            if !self.config.strategy.simulation_mode && (up_wins || down_wins) {
                let (token_id, outcome) = if up_wins && trade.up_shares > 0.001 {
                    (trade.up_token_id.as_deref().unwrap_or(""), "Up")
                } else {
                    (trade.down_token_id.as_deref().unwrap_or(""), "Down")
                };
                let _units = if up_wins { trade.up_shares } else { trade.down_shares };
                if let Err(e) = self
                    .api
                    .redeem_tokens(&trade.condition_id, token_id, outcome)
                    .await
                {
                    warn!("Redeem failed: {}", e);
                }
            }

            {
                let mut total = self.total_profit.lock().await;
                *total += pnl;
            }
            {
                let mut period = self.period_profit.lock().await;
                *period += pnl;
            }
            let total_actual_pnl = *self.total_profit.lock().await;
            eprintln!(
                "  -> Actual PnL this market: ${:.2} | Total actual PnL (all time): ${:.2}",
                pnl,
                total_actual_pnl
            );
            {
                let mut c = self.closure_checked.lock().await;
                c.insert(trade.condition_id.clone(), true);
            }
            let mut t = self.trades.lock().await;
            t.remove(&market_key);
        }
        Ok(())
    }

    fn round_price(price: f64) -> f64 {
        let rounded = (price * 100.0).round() / 100.0;
        rounded.clamp(0.01, 0.99)
    }

    fn cycle_trade_holding_winner(s: &PreLimitOrderState, winner: &str, shares: f64) -> CycleTrade {
        let (up_shares, down_shares, up_avg, down_avg) = if winner == "Up" {
            (shares, 0.0, s.up_order_price, 0.0)
        } else {
            (0.0, shares, 0.0, s.down_order_price)
        };
        CycleTrade {
            condition_id: s.condition_id.clone(),
            period_timestamp: s.market_period_start as u64,
            market_duration_secs: 3600,
            up_token_id: Some(s.up_token_id.clone()),
            down_token_id: Some(s.down_token_id.clone()),
            up_shares,
            down_shares,
            up_avg_price: up_avg,
            down_avg_price: down_avg,
        }
    }

    fn cycle_trade_holding_both(s: &PreLimitOrderState, shares: f64) -> CycleTrade {
        CycleTrade {
            condition_id: s.condition_id.clone(),
            period_timestamp: s.market_period_start as u64,
            market_duration_secs: 3600,
            up_token_id: Some(s.up_token_id.clone()),
            down_token_id: Some(s.down_token_id.clone()),
            up_shares: shares,
            down_shares: shares,
            up_avg_price: s.up_order_price,
            down_avg_price: s.down_order_price,
        }
    }

    async fn place_limit_order(&self, token_id: &str, side: &str, price: f64) -> Result<OrderResponse> {
        let price = Self::round_price(price);
        if self.config.strategy.simulation_mode {
            log::info!("🎮 SIMULATION: Would place {} order for token {}: {} shares @ ${:.2}", 
                side, token_id, self.config.strategy.shares, price);
            
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
                price: price.to_string(),
                order_type: "LIMIT".to_string(),
            };
            self.api.place_order(&order).await
        }
    }

    async fn check_order_matches(&self, state: &mut PreLimitOrderState) -> Result<()> {
        let current_time_et = Self::get_current_time_et();
        
        // IMPORTANT: Only check matches if the market where orders were placed has actually started
        if current_time_et < state.market_period_start {
            log::debug!("Market {} for {} hasn't started yet (current: {}, start: {})", 
                state.market_period_start, state.asset, current_time_et, state.market_period_start);
            return Ok(());
        }

        // Production: verify fill status via CLOB API (ground truth). Simulation: infer from price.
        if !self.config.strategy.simulation_mode {
            if let (Some(up_id), Some(down_id)) = (&state.up_order_id, &state.down_order_id) {
                // Skip API for simulation-style fake order IDs
                if !up_id.starts_with("SIM-") && !down_id.starts_with("SIM-") {
                    match self.api.are_both_orders_filled(up_id, down_id).await {
                        Ok((up_filled, down_filled)) => {
                            if up_filled && !state.up_matched {
                                log::info!("✅ Up order filled for {} (verified via API)", state.asset);
                                state.up_matched = true;
                            }
                            if down_filled && !state.down_matched {
                                log::info!("✅ Down order filled for {} (verified via API)", state.asset);
                                state.down_matched = true;
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            log::debug!("{}: API fill check failed ({}), falling back to price inference", state.asset, e);
                        }
                    }
                }
            }
        }

        // Simulation or API fallback: infer matched from current price vs limit
        let up_price_result = self.api.get_price(&state.up_token_id, "SELL").await;
        let down_price_result = self.api.get_price(&state.down_token_id, "SELL").await;
        
        if let Ok(up_price) = up_price_result {
            let up_price_f64: f64 = up_price.to_string().parse().unwrap_or(0.0);
            let limit = state.up_order_price;
            if (up_price_f64 <= limit || (up_price_f64 - limit).abs() < 0.001) && !state.up_matched {
                if self.config.strategy.simulation_mode {
                    log::info!("🎮 SIMULATION: Up order matched for {} (price hit ${:.4} <= ${:.2})", 
                        state.asset, up_price_f64, limit);
                } else {
                    log::info!("✅ Up order matched for {} (price hit ${:.4} <= ${:.2})", 
                        state.asset, up_price_f64, limit);
                }
                state.up_matched = true;
            }
        }
        
        if let Ok(down_price) = down_price_result {
            let down_price_f64: f64 = down_price.to_string().parse().unwrap_or(0.0);
            let limit = state.down_order_price;
            let price_matches = down_price_f64 <= limit || (down_price_f64 - limit).abs() < 0.001;
            log::debug!("Checking Down order for {}: price=${:.2}, limit=${:.2}, matches={}", 
                state.asset, down_price_f64, limit, price_matches);
            if price_matches && !state.down_matched {
                if self.config.strategy.simulation_mode {
                    log::info!("🎮 SIMULATION: Down order matched for {} (price hit ${:.2} <= ${:.2})", 
                        state.asset, down_price_f64, limit);
                } else {
                    log::info!("✅ Down order matched for {} (price hit ${:.2} <= ${:.2})", 
                        state.asset, down_price_f64, limit);
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
            let asset_slug = ASSET_TO_SLUG
                .iter()
                .find(|(name, _)| *name == *asset)
                .map(|(_, slug)| *slug)
                .unwrap_or("bitcoin");
            
            if let Some(state) = states.get_mut(*asset) {
                let market_period = state.market_period_start;
                let slug = MarketDiscovery::build_1h_slug(asset_slug, market_period);
                
                match self.api.get_market_by_slug(&slug).await {
                    Ok(market) => {
                        if market.active && !market.closed {
                            let up_price_result = self.api.get_price(&state.up_token_id, "SELL").await;
                            let down_price_result = self.api.get_price(&state.down_token_id, "SELL").await;
                            
                            // Calculate remaining time for the market where orders were placed (1h = 3600s)
                            let market_end = market_period + 3600;
                            let time_remaining = market_end - current_time_et;
                            let minutes = if time_remaining > 0 { time_remaining / 60 } else { 0 };
                            let seconds = if time_remaining > 0 { time_remaining % 60 } else { 0 };

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
                            let up_limit = state.up_order_price;
                            let down_limit = state.down_order_price;
                            let up_price_matched = up_price_result.as_ref()
                                .ok()
                                .and_then(|p| p.to_string().parse::<f64>().ok())
                                .map(|p| p <= up_limit || (p - up_limit).abs() < 0.001)
                                .unwrap_or(false);
                            let down_price_matched = down_price_result.as_ref()
                                .ok()
                                .and_then(|p| p.to_string().parse::<f64>().ok())
                                .map(|p| p <= down_limit || (p - down_limit).abs() < 0.001)
                                .unwrap_or(false);

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
                let current_period_et = Self::get_current_1h_period_et();
                let slug = MarketDiscovery::build_1h_slug(asset_slug, current_period_et);
                log::debug!("Trying to find {} market with slug: {}", asset, slug);
                
                match self.api.get_market_by_slug(&slug).await {
                    Ok(market) => {
                        if market.active && !market.closed {
                            match self.api.get_market(&market.condition_id).await {
                                Ok(_) => {
                                    match self.discovery.get_market_tokens(&market.condition_id).await {
                                        Ok((up_token_id, down_token_id)) => {
                                            // Get prices via REST API
                                            let (up_price_result, down_price_result) = tokio::join!(
                                                self.api.get_price(&up_token_id, "SELL"),
                                                self.api.get_price(&down_token_id, "SELL")
                                            );
                                            
                                            let market_end = current_period_et + 3600;
                                            let time_remaining = market_end - current_time_et;
                                            let minutes = if time_remaining > 0 { time_remaining / 60 } else { 0 };
                                            let seconds = if time_remaining > 0 { time_remaining % 60 } else { 0 };

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
                    Err(e) => {
                        log::info!("{} | Current market not found (slug: {}, error: {})", asset, slug, e);
                    }
                }
            }
        }
        
        // States are already updated in the loop above (get_mut modifies in place)
        drop(states);
        log::info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

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

                if state.up_matched != before_up || state.down_matched != before_down {
                    log::debug!("State updated for {}: up_matched={}->{}, down_matched={}->{}", 
                        asset, before_up, state.up_matched, before_down, state.down_matched);
                }
            }
        }
        
        Ok(())
    }
}

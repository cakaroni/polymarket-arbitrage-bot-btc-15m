use crate::api::PolymarketApi;
use crate::models::*;
use anyhow::Result;
use log::{debug, warn, error};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;

pub struct MarketMonitor {
    api: Arc<PolymarketApi>,
    market_name: String,
    btc_market_15m: Arc<tokio::sync::Mutex<crate::models::Market>>,
    check_interval: Duration,
    data_source: String,
    btc_15m_up_token_id: Arc<tokio::sync::Mutex<Option<String>>>,
    btc_15m_down_token_id: Arc<tokio::sync::Mutex<Option<String>>>,
    last_market_refresh: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
    current_period_timestamp: Arc<tokio::sync::Mutex<u64>>,
    period_duration_secs: u64,
    clob_url: String,
    /// Period end (unix secs) from API; used when slug has no timestamp (e.g. 1h human-readable slugs).
    cached_period_end_timestamp: Arc<tokio::sync::Mutex<Option<u64>>>,
}

#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub market_name: String,
    pub btc_market_15m: MarketData,
    pub timestamp: std::time::Instant,
    pub btc_15m_time_remaining: u64,
    pub btc_15m_period_timestamp: u64,
    /// Market duration in seconds (900 for 15m, 3600 for 1h).
    pub market_duration_secs: u64,
}

impl MarketMonitor {
    pub fn new(
        api: Arc<PolymarketApi>,
        market_name: String,
        btc_market_15m: crate::models::Market,
        check_interval_ms: u64,
        data_source: String,
        clob_url: String,
    ) -> Self {
        let period_duration_secs = Self::extract_duration_from_slug(&btc_market_15m.slug);
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let current_period = (current_time / period_duration_secs) * period_duration_secs;
        
        Self {
            api,
            market_name,
            btc_market_15m: Arc::new(tokio::sync::Mutex::new(btc_market_15m)),
            check_interval: Duration::from_millis(check_interval_ms),
            data_source: data_source.to_lowercase(),
            btc_15m_up_token_id: Arc::new(tokio::sync::Mutex::new(None)),
            btc_15m_down_token_id: Arc::new(tokio::sync::Mutex::new(None)),
            last_market_refresh: Arc::new(tokio::sync::Mutex::new(None)),
            current_period_timestamp: Arc::new(tokio::sync::Mutex::new(current_period)),
            period_duration_secs,
            clob_url,
            cached_period_end_timestamp: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub async fn update_market(
        &self,
        btc_market_15m: crate::models::Market,
    ) -> Result<()> {
        eprintln!("Updating {} market...", self.market_name);
        eprintln!("New {} Market: {} ({})", self.market_name, btc_market_15m.slug, btc_market_15m.condition_id);
        let period_duration_secs = Self::extract_duration_from_slug(&btc_market_15m.slug);
        
        *self.btc_market_15m.lock().await = btc_market_15m;
        
        // Reset token IDs and cached period end (will be refilled on next refresh)
        *self.btc_15m_up_token_id.lock().await = None;
        *self.btc_15m_down_token_id.lock().await = None;
        *self.last_market_refresh.lock().await = None;
        *self.cached_period_end_timestamp.lock().await = None;
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let new_period = (current_time / period_duration_secs) * period_duration_secs;
        *self.current_period_timestamp.lock().await = new_period;
        
        Ok(())
    }


    pub async fn get_current_condition_id(&self) -> String {
        self.btc_market_15m.lock().await.condition_id.clone()
    }

    /// Get the current market's period start (unix seconds). For 1h human-readable slugs
    /// derives period end from slug (ET) then uses cache as fallback.
    pub async fn get_current_market_timestamp(&self) -> u64 {
        let btc_market = self.btc_market_15m.lock().await;
        let slug = btc_market.slug.clone();
        let duration = Self::extract_duration_from_slug(&slug);
        drop(btc_market);
        let from_slug = Self::extract_timestamp_from_slug(&slug);
        if from_slug != 0 {
            return from_slug;
        }
        if duration == 3600 {
            if let Some(end) = Self::period_end_from_1h_slug(&slug) {
                return end.saturating_sub(duration);
            }
        }
        let cache = self.cached_period_end_timestamp.lock().await;
        cache.map(|end| end.saturating_sub(duration)).unwrap_or(0)
    }

    async fn refresh_market_tokens(&self) -> Result<()> {
        let should_refresh = {
            let last_refresh = self.last_market_refresh.lock().await;
            last_refresh
                .map(|last| last.elapsed().as_secs() >= 900)
                .unwrap_or(true)
        };

        if !should_refresh {
            return Ok(());
        }

        let market_id = self.get_current_condition_id().await;
        eprintln!("{}: Refreshing tokens for market: {}", self.market_name, &market_id[..16]);

        // Get market details from CLOB
        match self.api.get_market(&market_id).await {
            Ok(btc_15m_details) => {
                if let Some(end_ts) = Self::parse_iso_to_unix(&btc_15m_details.end_date_iso) {
                    *self.cached_period_end_timestamp.lock().await = Some(end_ts);
                }
                for token in &btc_15m_details.tokens {
                    let outcome_upper = token.outcome.to_uppercase();
                    if outcome_upper.contains("UP") || outcome_upper == "1" {
                        *self.btc_15m_up_token_id.lock().await = Some(token.token_id.clone());
                        eprintln!("{} Up token_id: {}", self.market_name, token.token_id);
                    } else if outcome_upper.contains("DOWN") || outcome_upper == "0" {
                        *self.btc_15m_down_token_id.lock().await = Some(token.token_id.clone());
                        eprintln!("{} Down token_id: {}", self.market_name, token.token_id);
                    }
                }
            }
            Err(e) => {
                warn!("{}: Failed to fetch market details from CLOB (token IDs will be N/A until next refresh): {}", self.market_name, e);
                eprintln!("{}: CLOB get_market FAILED: {} (prices will show N/A until CLOB returns this market)", self.market_name, e);
            }
        }

        // Only throttle refresh when we got both token IDs; otherwise retry on next poll
        let up = self.btc_15m_up_token_id.lock().await.is_some();
        let down = self.btc_15m_down_token_id.lock().await.is_some();
        if up && down {
            *self.last_market_refresh.lock().await = Some(std::time::Instant::now());
        }
        Ok(())
    }

    pub async fn fetch_market_data(&self) -> Result<MarketSnapshot> {
        self.refresh_market_tokens().await?;

        let btc_15m_guard = self.btc_market_15m.lock().await;
        let btc_15m_slug = btc_15m_guard.slug.clone();
        let btc_15m_id = btc_15m_guard.condition_id.clone();
        drop(btc_15m_guard);

        let mut btc_15m_timestamp = Self::extract_timestamp_from_slug(&btc_15m_slug);
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let btc_15m_duration = Self::extract_duration_from_slug(&btc_15m_slug);
        let market_duration_secs = btc_15m_duration;
        if btc_15m_timestamp == 0 {
            // 1h markets: derive period end from slug (ET) so remaining time is correct
            if btc_15m_duration == 3600 {
                if let Some(end) = Self::period_end_from_1h_slug(&btc_15m_slug) {
                    btc_15m_timestamp = end.saturating_sub(btc_15m_duration);
                }
            }
            if btc_15m_timestamp == 0 {
                if let Some(end) = *self.cached_period_end_timestamp.lock().await {
                    btc_15m_timestamp = end.saturating_sub(btc_15m_duration);
                }
            }
        }
        let btc_15m_period_end = btc_15m_timestamp + btc_15m_duration;
        let btc_15m_remaining = if btc_15m_period_end > current_timestamp {
            btc_15m_period_end - current_timestamp
        } else { 0 };
        
        // Fetch prices
        let btc_15m_up_token_id = self.btc_15m_up_token_id.lock().await.clone();
        let btc_15m_down_token_id = self.btc_15m_down_token_id.lock().await.clone();
        
        // Always fetch prices so we show BID/ASK when CLOB has them (even if remaining is 0)
        let (btc_15m_up_price, btc_15m_down_price) = tokio::join!(
            self.fetch_token_price(&btc_15m_up_token_id, &self.market_name, "Up"),
            self.fetch_token_price(&btc_15m_down_token_id, &self.market_name, "Down"),
        );
        
        // Format remaining time
        let format_remaining_time = |secs: u64| -> String {
            if secs == 0 {
                "0s".to_string()
            } else {
                let minutes = secs / 60;
                let seconds = secs % 60;
                if minutes > 0 {
                    format!("{}m {}s", minutes, seconds)
                } else {
                    format!("{}s", seconds)
                }
            }
        };
        
        let btc_15m_remaining_str = format_remaining_time(btc_15m_remaining);
        let format_price_with_both = |p: &TokenPrice| -> String {
            let bid = p.bid.unwrap_or(rust_decimal::Decimal::ZERO);
            let ask = p.ask.unwrap_or(rust_decimal::Decimal::ZERO);
            let bid_f64: f64 = bid.to_string().parse().unwrap_or(0.0);
            let ask_f64: f64 = ask.to_string().parse().unwrap_or(0.0);
            format!("BID:${:.2} ASK:${:.2}", bid_f64, ask_f64)
        };

        let btc_15m_up_str = btc_15m_up_price.as_ref()
            .map(format_price_with_both)
            .unwrap_or_else(|| "N/A".to_string());
        let btc_15m_down_str = btc_15m_down_price.as_ref()
            .map(format_price_with_both)
            .unwrap_or_else(|| "N/A".to_string());
        
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S");
        let message = format!(
            "[{}] {} Up Token {} Down Token {} remaining time:{}\n",
            ts, self.market_name, btc_15m_up_str, btc_15m_down_str, btc_15m_remaining_str
        );
        crate::log_to_history(&message);

        let btc_15m_market_data = MarketData {
            condition_id: btc_15m_id,
            market_name: self.market_name.clone(),
            up_token: btc_15m_up_price,
            down_token: btc_15m_down_price,
        };

        Ok(MarketSnapshot {
            market_name: self.market_name.clone(),
            btc_market_15m: btc_15m_market_data,
            timestamp: std::time::Instant::now(),
            btc_15m_time_remaining: btc_15m_remaining,
            btc_15m_period_timestamp: btc_15m_timestamp,
            market_duration_secs,
        })
    }

    async fn fetch_token_price(
        &self,
        token_id: &Option<String>,
        market_name: &str,
        outcome: &str,
    ) -> Option<TokenPrice> {
        let token_id = token_id.as_ref()?;

        let buy_price = match self.api.get_price(token_id, "BUY").await {
            Ok(price) => Some(price),
            Err(e) => {
                warn!("Failed to fetch {} {} BUY price: {}", market_name, outcome, e);
                None
            }
        };

        let sell_price = match self.api.get_price(token_id, "SELL").await {
            Ok(price) => Some(price),
            Err(e) => {
                warn!("Failed to fetch {} {} SELL price: {}", market_name, outcome, e);
                None
            }
        };

        if buy_price.is_some() || sell_price.is_some() {
            Some(TokenPrice {
                token_id: token_id.clone(),
                bid: buy_price,  // BID = BUY price 
                ask: sell_price, // ASK = SELL price 
            })
        } else {
            None
        }
    }

    pub fn extract_timestamp_from_slug(slug: &str) -> u64 {
        if let Some(last_dash) = slug.rfind('-') {
            if let Ok(timestamp) = slug[last_dash + 1..].parse::<u64>() {
                return timestamp;
            }
        }
        0
    }

    /// Parse ISO 8601 / RFC3339 date string to unix timestamp (seconds). Used for end_date_iso from API.
    fn parse_iso_to_unix(s: &str) -> Option<u64> {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc).timestamp())
            .and_then(|t| if t >= 0 { Some(t as u64) } else { None })
    }
    
    pub fn extract_duration_from_slug(slug: &str) -> u64 {
        if slug.contains("-15m-") {
            900
        } else if slug.contains("-1h-") || (slug.contains("up-or-down") && slug.ends_with("-et")) {
            3600
        } else {
            900
        }
    }

    /// Parse 1h market slug (e.g. bitcoin-up-or-down-february-3-11am-et) and return period end
    /// as Unix timestamp. Slug encodes period *start* in ET; period end = start + 1 hour.
    pub fn period_end_from_1h_slug(slug: &str) -> Option<u64> {
        if !slug.ends_with("-et") || !slug.contains("up-or-down") {
            return None;
        }
        let rest = slug.strip_suffix("-et")?.trim_end_matches('-');
        let after = rest.split("up-or-down-").nth(1)?; // "february-3-11am"
        let parts: Vec<&str> = after.split('-').collect();
        if parts.len() < 3 {
            return None;
        }
        let month_name = parts[0].to_lowercase();
        let day: u32 = parts[1].parse().ok()?;
        let hour_ampm = parts[2].to_lowercase();
        let month = match month_name.as_str() {
            "january" => 1, "february" => 2, "march" => 3, "april" => 4, "may" => 5, "june" => 6,
            "july" => 7, "august" => 8, "september" => 9, "october" => 10, "november" => 11, "december" => 12,
            _ => return None,
        };
        let (hour_24, _) = if hour_ampm.ends_with("am") {
            let h: u32 = hour_ampm.trim_end_matches("am").parse().ok()?;
            (if h == 12 { 0 } else { h }, ())
        } else if hour_ampm.ends_with("pm") {
            let h: u32 = hour_ampm.trim_end_matches("pm").parse().ok()?;
            (if h == 12 { 12 } else { h + 12 }, ())
        } else {
            return None;
        };
        let year = Utc::now().year();
        let date = NaiveDate::from_ymd_opt(year, month, day)?;
        let time = NaiveTime::from_hms_opt(hour_24, 0, 0)?;
        let naive_dt = NaiveDateTime::new(date, time);
        let et_dt = New_York.from_local_datetime(&naive_dt).single()?;
        let period_start_ts = et_dt.timestamp();
        let period_end_ts = period_start_ts + 3600;
        if period_end_ts < 0 {
            return None;
        }
        Some(period_end_ts as u64)
    }

    pub async fn start_monitoring<F, Fut>(&self, callback: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        if self.data_source == "websocket" {
            eprintln!("Starting market monitoring via WebSocket...");
            self.start_websocket_monitoring(callback).await;
        } else {
            eprintln!("Starting market monitoring via API polling...");
            self.start_api_monitoring(callback).await;
        }
    }

    async fn start_api_monitoring<F, Fut>(&self, callback: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        loop {
            match self.fetch_market_data().await {
                Ok(snapshot) => {
                    debug!("Market snapshot updated");
                    callback(snapshot).await;
                }
                Err(e) => {
                    warn!("Error fetching market data: {}", e);
                }
            }
            
            sleep(self.check_interval).await;
        }
    }

    async fn start_websocket_monitoring<F, Fut>(&self, callback: F)
    where
        F: Fn(MarketSnapshot) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MarketSnapshot>();
        let callback_arc = Arc::new(callback);
        
        let callback_task = {
            let callback_ref = callback_arc.clone();
            tokio::spawn(async move {
                while let Some(snapshot) = rx.recv().await {
                    callback_ref(snapshot).await;
                }
            })
        };

        loop {
            match self.refresh_market_tokens().await {
                Ok(_) => {}
                Err(e) => {
                    warn!("Failed to refresh market tokens: {}", e);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }

            let up_token_id = self.btc_15m_up_token_id.lock().await.clone();
            let down_token_id = self.btc_15m_down_token_id.lock().await.clone();

            if up_token_id.is_none() || down_token_id.is_none() {
                warn!("Token IDs not available, falling back to API polling");
                sleep(Duration::from_secs(5)).await;
                continue;
            }

            let ws_url = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
            eprintln!("Connecting to WebSocket: {}", ws_url);

            match connect_async(ws_url).await {
                Ok((ws_stream, _)) => {
                    eprintln!("WebSocket connected successfully");
                    let (mut write, mut read) = ws_stream.split();

                    let up_id = up_token_id.clone().unwrap();
                    let down_id = down_token_id.clone().unwrap();

                    let subscribe_msg = json!({
                        "assets_ids": [up_id.clone(), down_id.clone()],
                        "type": "market"
                    });

                    if let Err(e) = write.send(Message::Text(subscribe_msg.to_string())).await {
                        error!("Failed to send subscribe message: {}", e);
                        sleep(Duration::from_secs(5)).await;
                        continue;
                    }

                    let mut last_snapshot_time = std::time::Instant::now();
                    let snapshot_interval = self.check_interval;
                    let mut up_price: Option<TokenPrice> = None;
                    let mut down_price: Option<TokenPrice> = None;
                    let tx_send = tx.clone();
                    let mut last_ping = std::time::Instant::now();

                    loop {
                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        if text == "PONG" {
                                            continue;
                                        }
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                                            if let Some(prices) = self.parse_websocket_message(&json, &up_id, &down_id).await {
                                                if let Some(up) = prices.0 {
                                                    up_price = Some(up);
                                                }
                                                if let Some(down) = prices.1 {
                                                    down_price = Some(down);
                                                }
                                                
                                                if last_snapshot_time.elapsed() >= snapshot_interval {
                                                    if let Ok(snapshot) = self.create_snapshot_from_prices(up_price.clone(), down_price.clone()).await {
                                                        let _ = tx_send.send(snapshot);
                                                        last_snapshot_time = std::time::Instant::now();
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Pong(_))) => {
                                        continue;
                                    }
                                    Some(Ok(Message::Close(_))) => {
                                        warn!("WebSocket connection closed");
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        error!("WebSocket error: {}", e);
                                        break;
                                    }
                                    None => {
                                        warn!("WebSocket stream ended");
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            _ = sleep(Duration::from_secs(1)) => {
                                if last_ping.elapsed() >= Duration::from_secs(5) {
                                    if let Err(e) = write.send(Message::Text("PING".to_string())).await {
                                        error!("Failed to send PING: {}", e);
                                        break;
                                    }
                                    last_ping = std::time::Instant::now();
                                }
                                
                                if last_snapshot_time.elapsed() >= snapshot_interval {
                                    if let Ok(snapshot) = self.fetch_market_data().await {
                                        let _ = tx_send.send(snapshot);
                                        last_snapshot_time = std::time::Instant::now();
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to connect to WebSocket: {}", e);
                    warn!("Falling back to API polling for 30 seconds...");
                    sleep(Duration::from_secs(30)).await;
                }
            }
        }
        
        drop(tx);
        let _ = callback_task.await;
    }


    async fn parse_websocket_message(
        &self,
        json: &serde_json::Value,
        up_token_id: &str,
        down_token_id: &str,
    ) -> Option<(Option<TokenPrice>, Option<TokenPrice>)> {
        let event_type = json.get("event_type").and_then(|v| v.as_str());
        
        match event_type {
            Some("best_bid_ask") => {
                if let Some(asset_id) = json.get("asset_id").and_then(|v| v.as_str()) {
                    let best_bid = json.get("best_bid")
                        .and_then(|v| v.as_str())
                        .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok());
                    let best_ask = json.get("best_ask")
                        .and_then(|v| v.as_str())
                        .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok());
                    
                    if asset_id == up_token_id {
                        return Some((
                            Some(TokenPrice {
                                token_id: up_token_id.to_string(),
                                bid: best_bid,
                                ask: best_ask,
                            }),
                            None,
                        ));
                    } else if asset_id == down_token_id {
                        return Some((
                            None,
                            Some(TokenPrice {
                                token_id: down_token_id.to_string(),
                                bid: best_bid,
                                ask: best_ask,
                            }),
                        ));
                    }
                }
            }
            Some("price_change") => {
                if let Some(price_changes) = json.get("price_changes").and_then(|v| v.as_array()) {
                    for change in price_changes {
                        if let Some(asset_id) = change.get("asset_id").and_then(|v| v.as_str()) {
                            let best_bid = change.get("best_bid")
                                .and_then(|v| v.as_str())
                                .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok());
                            let best_ask = change.get("best_ask")
                                .and_then(|v| v.as_str())
                                .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok());
                            
                            if asset_id == up_token_id {
                                return Some((
                                    Some(TokenPrice {
                                        token_id: up_token_id.to_string(),
                                        bid: best_bid,
                                        ask: best_ask,
                                    }),
                                    None,
                                ));
                            } else if asset_id == down_token_id {
                                return Some((
                                    None,
                                    Some(TokenPrice {
                                        token_id: down_token_id.to_string(),
                                        bid: best_bid,
                                        ask: best_ask,
                                    }),
                                ));
                            }
                        }
                    }
                }
            }
            Some("book") => {
                if let Some(asset_id) = json.get("asset_id").and_then(|v| v.as_str()) {
                    let best_bid = json.get("bids")
                        .and_then(|v| v.as_array())
                        .and_then(|bids| bids.first())
                        .and_then(|bid| bid.get("price"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok());
                    let best_ask = json.get("asks")
                        .and_then(|v| v.as_array())
                        .and_then(|asks| asks.first())
                        .and_then(|ask| ask.get("price"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok());
                    
                    if asset_id == up_token_id {
                        return Some((
                            Some(TokenPrice {
                                token_id: up_token_id.to_string(),
                                bid: best_bid,
                                ask: best_ask,
                            }),
                            None,
                        ));
                    } else if asset_id == down_token_id {
                        return Some((
                            None,
                            Some(TokenPrice {
                                token_id: down_token_id.to_string(),
                                bid: best_bid,
                                ask: best_ask,
                            }),
                        ));
                    }
                }
            }
            _ => {}
        }
        None
    }

    async fn create_snapshot_from_prices(
        &self,
        up_price: Option<TokenPrice>,
        down_price: Option<TokenPrice>,
    ) -> Result<MarketSnapshot> {
        let btc_15m_guard = self.btc_market_15m.lock().await;
        let btc_15m_slug = btc_15m_guard.slug.clone();
        let btc_15m_id = btc_15m_guard.condition_id.clone();
        drop(btc_15m_guard);

        let mut btc_15m_timestamp = Self::extract_timestamp_from_slug(&btc_15m_slug);
        let current_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let btc_15m_duration = Self::extract_duration_from_slug(&btc_15m_slug);
        if btc_15m_timestamp == 0 {
            if btc_15m_duration == 3600 {
                if let Some(end) = Self::period_end_from_1h_slug(&btc_15m_slug) {
                    btc_15m_timestamp = end.saturating_sub(btc_15m_duration);
                }
            }
            if btc_15m_timestamp == 0 {
                if let Some(end) = *self.cached_period_end_timestamp.lock().await {
                    btc_15m_timestamp = end.saturating_sub(btc_15m_duration);
                }
            }
        }
        let btc_15m_period_end = btc_15m_timestamp + btc_15m_duration;
        let btc_15m_remaining = if btc_15m_period_end > current_timestamp {
            btc_15m_period_end - current_timestamp
        } else { 0 };

        let format_remaining_time = |secs: u64| -> String {
            if secs == 0 {
                "0s".to_string()
            } else {
                let minutes = secs / 60;
                let seconds = secs % 60;
                if minutes > 0 {
                    format!("{}m {}s", minutes, seconds)
                } else {
                    format!("{}s", seconds)
                }
            }
        };

        let btc_15m_remaining_str = format_remaining_time(btc_15m_remaining);
        let format_price_with_both = |p: &TokenPrice| -> String {
            let bid = p.bid.unwrap_or(rust_decimal::Decimal::ZERO);
            let ask = p.ask.unwrap_or(rust_decimal::Decimal::ZERO);
            let bid_f64: f64 = bid.to_string().parse().unwrap_or(0.0);
            let ask_f64: f64 = ask.to_string().parse().unwrap_or(0.0);
            format!("BID:${:.2} ASK:${:.2}", bid_f64, ask_f64)
        };

        let btc_15m_up_str = up_price.as_ref()
            .map(format_price_with_both)
            .unwrap_or_else(|| "N/A".to_string());
        let btc_15m_down_str = down_price.as_ref()
            .map(format_price_with_both)
            .unwrap_or_else(|| "N/A".to_string());
        
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S");
        let message = format!(
            "[{}] {} Up Token {} Down Token {} remaining time:{}\n",
            ts, self.market_name, btc_15m_up_str, btc_15m_down_str, btc_15m_remaining_str
        );
        crate::log_to_history(&message);

        let btc_15m_market_data = MarketData {
            condition_id: btc_15m_id,
            market_name: self.market_name.clone(),
            up_token: up_price,
            down_token: down_price,
        };

        Ok(MarketSnapshot {
            market_name: self.market_name.clone(),
            btc_market_15m: btc_15m_market_data,
            timestamp: std::time::Instant::now(),
            btc_15m_time_remaining: btc_15m_remaining,
            btc_15m_period_timestamp: btc_15m_timestamp,
            market_duration_secs: btc_15m_duration,
        })
    }
}


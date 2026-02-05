//! Trend-based strategy:
//! - Monitor price over short window (4–5 data points). If one side is rising → buy that side (2–3 times). If flat → buy higher side once.
//! - When we can lock PnL (cost per pair ≤ max) by buying the other side → buy it (lock).
//! - After lock, monitor again: if the other side is falling or flat → buy our side again; repeat.
//! PnL is calculated only after market closes (same in simulation and production).

use crate::api::PolymarketApi;
use crate::monitor::MarketSnapshot;
use anyhow::Result;
use log::warn;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

const PRICE_HISTORY_LEN: usize = 5;
const TREND_THRESHOLD: f64 = 0.005;
const MAX_RISING_BUYS_PER_WAVE: u32 = 3;
/// When no position and trend is rising: buy the rising side at most 1–2 times.
const MAX_RISING_BUYS_NO_POSITION: u32 = 2;
/// When no position and trend is flat: buy the higher-priced side up to 3–4 times.
const MAX_FLAT_BUYS_NO_POSITION: u32 = 4;
/// When rebalancing PnL (buying the side with worse outcome), allow cost per pair up to this.
const REBALANCE_COST_PER_PAIR_MAX: f64 = 1.02;
/// Max buys of one side when rebalancing PnL (outcome skewed); can be higher than trend-follow limit.
const MAX_REBALANCE_BUYS: u32 = 8;

#[derive(Debug, Clone, Default)]
struct WaveState {
    buys_up_since_lock: u32,
    buys_down_since_lock: u32,
    /// When no position and flat, how many "buy higher side" we did (max MAX_FLAT_BUYS_NO_POSITION).
    flat_buys_since_lock: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Trend {
    UpRising,
    DownRising,
    Flat,
    /// Down price falling (up_ask rising)
    DownFalling,
    /// Up price falling (down_ask rising)
    UpFalling,
}

pub struct Trader {
    api: Arc<PolymarketApi>,
    simulation_mode: bool,
    cost_per_pair_max: f64,
    min_side_price: f64,
    max_side_price: f64,
    cooldown_seconds: u64,
    cooldown_seconds_1h: u64,
    shares_override: Option<f64>,
    size_reduce_after_secs: u64,
    size_min_ratio: f64,
    size_min_shares: f64,
    last_buy: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    trades: Arc<Mutex<HashMap<String, CycleTrade>>>,
    total_profit: Arc<Mutex<f64>>,
    period_profit: Arc<Mutex<f64>>,
    closure_checked: Arc<Mutex<HashMap<String, bool>>>,
    price_history: Arc<Mutex<HashMap<String, VecDeque<(u64, f64, f64)>>>>,
    wave_state: Arc<Mutex<HashMap<String, WaveState>>>,
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

impl Trader {
    pub fn new(
        api: Arc<PolymarketApi>,
        simulation_mode: bool,
        cost_per_pair_max: f64,
        min_side_price: f64,
        max_side_price: f64,
        cooldown_seconds: u64,
        cooldown_seconds_1h: u64,
        shares_override: Option<f64>,
        size_reduce_after_secs: u64,
        size_min_ratio: f64,
        size_min_shares: f64,
    ) -> Self {
        Self {
            api,
            simulation_mode,
            cost_per_pair_max,
            min_side_price,
            max_side_price,
            cooldown_seconds,
            cooldown_seconds_1h,
            shares_override,
            size_reduce_after_secs,
            size_min_ratio,
            size_min_shares,
            last_buy: Arc::new(Mutex::new(HashMap::new())),
            trades: Arc::new(Mutex::new(HashMap::new())),
            total_profit: Arc::new(Mutex::new(0.0)),
            period_profit: Arc::new(Mutex::new(0.0)),
            closure_checked: Arc::new(Mutex::new(HashMap::new())),
            price_history: Arc::new(Mutex::new(HashMap::new())),
            wave_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Update price history and return current trend (need at least 4 points).
    async fn update_trend(
        &self,
        market_key: &str,
        current_time: u64,
        up_ask: f64,
        down_ask: f64,
    ) -> Trend {
        let mut hist = self.price_history.lock().await;
        let entry = hist.entry(market_key.to_string()).or_default();
        entry.push_back((current_time, up_ask, down_ask));
        while entry.len() > PRICE_HISTORY_LEN {
            entry.pop_front();
        }
        let len = entry.len();
        let first = entry.front().copied().unwrap_or((0, 0.0, 0.0));
        let last = entry.back().copied().unwrap_or((0, 0.0, 0.0));
        drop(hist);

        if len < 4 {
            return Trend::Flat;
        }
        let up_delta = last.1 - first.1;
        let down_delta = last.2 - first.2;
        if up_delta >= TREND_THRESHOLD && up_delta >= down_delta {
            Trend::UpRising
        } else if down_delta >= TREND_THRESHOLD && down_delta >= up_delta {
            Trend::DownRising
        } else if down_delta <= -TREND_THRESHOLD && down_delta <= up_delta {
            Trend::DownFalling
        } else if up_delta <= -TREND_THRESHOLD && up_delta <= down_delta {
            Trend::UpFalling
        } else {
            Trend::Flat
        }
    }

    /// Base size per market (no time reduction).
    fn base_shares_for_market(&self, market_name: &str) -> f64 {
        if let Some(s) = self.shares_override {
            if s > 0.0 {
                return s;
            }
        }
        let upper = market_name.to_uppercase();
        if upper.starts_with("BTC") && upper.contains("15") {
            24.0
        } else if upper.starts_with("ETH") && upper.contains("15") {
            14.0
        } else if upper.starts_with("BTC") && (upper.contains("1H") || upper.contains("1 H")) {
            26.0
        } else if upper.starts_with("ETH") && (upper.contains("1H") || upper.contains("1 H")) {
            16.0
        } else {
            24.0
        }
    }

    /// Size to use for this snapshot: reduce toward market end (volatility). Target does this.
    fn shares_for_market_with_time(
        &self,
        market_name: &str,
        time_remaining_secs: u64,
        _market_duration_secs: u64,
    ) -> f64 {
        let base = self.base_shares_for_market(market_name);
        if self.size_reduce_after_secs == 0 || time_remaining_secs >= self.size_reduce_after_secs {
            return base;
        }
        let ratio = self.size_min_ratio
            + (1.0 - self.size_min_ratio) * (time_remaining_secs as f64 / self.size_reduce_after_secs as f64);
        let size = (base * ratio * 100.0).round() / 100.0;
        size.max(self.size_min_shares)
    }

    pub async fn process_snapshot(&self, snapshot: &MarketSnapshot) -> Result<()> {
        let market_name = &snapshot.market_name;
        let market_data = &snapshot.btc_market_15m;
        let period_timestamp = snapshot.btc_15m_period_timestamp;
        let condition_id = &market_data.condition_id;
        let time_remaining = snapshot.btc_15m_time_remaining;

        if time_remaining == 0 {
            return Ok(());
        }

        let up_ask = market_data
            .up_token
            .as_ref()
            .and_then(|t| t.ask_price().to_string().parse::<f64>().ok())
            .unwrap_or(0.0);
        let down_ask = market_data
            .down_token
            .as_ref()
            .and_then(|t| t.ask_price().to_string().parse::<f64>().ok())
            .unwrap_or(0.0);

        if up_ask <= 0.0 || down_ask <= 0.0 {
            return Ok(());
        }

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let market_key = format!("{}:{}", condition_id, period_timestamp);
        let (up_shares, down_shares, up_avg, down_avg) = {
            let t = self.trades.lock().await;
            t.get(&market_key)
                .map(|e| (e.up_shares, e.down_shares, e.up_avg_price, e.down_avg_price))
                .unwrap_or((0.0, 0.0, 0.0, 0.0))
        };
        let up_cost = up_shares * up_avg;
        let down_cost = down_shares * down_avg;
        let total_cost = up_cost + down_cost;

        let duration_secs = snapshot.market_duration_secs;
        let size = self.shares_for_market_with_time(market_name, time_remaining, duration_secs);

        // Update price history and get trend (4–5 data points)
        let trend = self.update_trend(&market_key, current_time, up_ask, down_ask).await;
        let wave = self.wave_state.lock().await;
        let wave_state = wave.get(&market_key).cloned().unwrap_or_default();
        drop(wave);

        let up_price_ok = up_ask >= self.min_side_price && up_ask <= self.max_side_price;
        let down_price_ok = down_ask >= self.min_side_price && down_ask <= self.max_side_price;

        let current_pairs = up_shares.min(down_shares);
        let _current_cost_per_pair = if current_pairs > 0.0 {
            total_cost / current_pairs
        } else {
            f64::MAX
        };

        let new_up = up_shares + size;
        let new_up_cost = up_cost + size * up_ask;
        let pairs_after_up = new_up.min(down_shares);
        // When we have more Down than Up, only the paired Up cost counts (marginal cost per pair).
        let cost_per_pair_up = if pairs_after_up > 0.0 {
            if down_shares >= new_up {
                (pairs_after_up * down_avg + new_up_cost) / pairs_after_up
            } else {
                (new_up_cost + down_cost) / pairs_after_up
            }
        } else {
            f64::MAX
        };

        let new_down = down_shares + size;
        let new_down_cost = down_cost + size * down_ask;
        let pairs_after_down = up_shares.min(new_down);
        // When we have more Up than Down, only the paired Up cost counts (marginal cost per pair).
        let cost_per_pair_down = if pairs_after_down > 0.0 {
            if up_shares >= new_down {
                (pairs_after_down * up_avg + new_down_cost) / pairs_after_down
            } else {
                (up_cost + new_down_cost) / pairs_after_down
            }
        } else {
            f64::MAX
        };

        // Trend-based decision
        let can_lock_with_up = down_shares > 0.0 && pairs_after_up > 0.0 && cost_per_pair_up <= self.cost_per_pair_max;
        let can_lock_with_down = up_shares > 0.0 && pairs_after_down > 0.0 && cost_per_pair_down <= self.cost_per_pair_max;

        // PnL if each outcome wins: payout is that side's shares at $1 each.
        let pnl_if_up_wins = up_shares - total_cost;
        let pnl_if_down_wins = down_shares - total_cost;

        let (do_buy_up, do_buy_down, is_lock) = if !up_price_ok && !down_price_ok {
            (false, false, false)
        } else if up_shares == 0.0 && down_shares == 0.0 {
            // No position: rising → buy rising side 1–2x; flat → buy higher-priced side 3–4x
            match trend {
                Trend::UpRising if up_price_ok && wave_state.buys_up_since_lock < MAX_RISING_BUYS_NO_POSITION => (true, false, false),
                Trend::DownRising if down_price_ok && wave_state.buys_down_since_lock < MAX_RISING_BUYS_NO_POSITION => (false, true, false),
                Trend::Flat | Trend::UpFalling | Trend::DownFalling if wave_state.flat_buys_since_lock < MAX_FLAT_BUYS_NO_POSITION => {
                    if up_ask >= down_ask && up_price_ok {
                        (true, false, false)
                    } else if down_price_ok {
                        (false, true, false)
                    } else {
                        (false, false, false)
                    }
                }
                _ => (false, false, false),
            }
        } else if down_shares > 0.0 && up_shares == 0.0 {
            // Only Down: 1) lock with Up when cost per pair ≤ max; 2) Expansion: can't lock + UpRising + PnL Up worse → buy Up; 3) else buy Down if Down rising
            if can_lock_with_up && up_price_ok {
                (true, false, true)
            } else if !can_lock_with_up && trend == Trend::UpRising && pnl_if_up_wins < pnl_if_down_wins && up_price_ok && wave_state.buys_up_since_lock < MAX_REBALANCE_BUYS {
                // Expansion (Example 3 mirror): no lock with Up, Up rising, PnL if Up wins worse → buy Up to create position
                (true, false, false)
            } else if trend == Trend::DownRising && down_price_ok && wave_state.buys_down_since_lock < MAX_RISING_BUYS_PER_WAVE {
                (false, true, false)
            } else {
                (false, false, false)
            }
        } else if up_shares > 0.0 && down_shares == 0.0 {
            // Only Up: 1) lock with Down when cost per pair ≤ max; 2) Expansion (Example 3): can't lock + DownRising + PnL Down worse → buy Down; 3) else buy Up if Up rising
            if can_lock_with_down && down_price_ok {
                (false, true, true)
            } else if !can_lock_with_down && trend == Trend::DownRising && pnl_if_down_wins < pnl_if_up_wins && down_price_ok && wave_state.buys_down_since_lock < MAX_REBALANCE_BUYS {
                // Expansion: no matching Up can lock with Down @ current price, Down rising, PnL if Down wins lower → buy Down
                (false, true, false)
            } else if trend == Trend::UpRising && up_price_ok && wave_state.buys_up_since_lock < MAX_RISING_BUYS_PER_WAVE {
                (true, false, false)
            } else {
                (false, false, false)
            }
        } else {
            // Have both: 1) lock when cost per pair ≤ max; 2) Expansion (Example 5): can't lock + rising side PnL worse → buy that side; 3) ride winner; 4) PnL rebalance; 5) trend (not Flat — Example 6)
            let underweight_down = up_shares > down_shares && can_lock_with_down && down_price_ok;
            let underweight_up = down_shares > up_shares && can_lock_with_up && up_price_ok;
            if underweight_down {
                (false, true, true)
            } else if underweight_up {
                (true, false, true)
            } else if trend != Trend::Flat && !can_lock_with_down && trend == Trend::DownRising && pnl_if_down_wins < pnl_if_up_wins && down_price_ok && wave_state.buys_down_since_lock < MAX_REBALANCE_BUYS {
                // Expansion (Example 5): can't lock with Down, Down rising, PnL if Down wins worse → buy Down till it improves
                (false, true, false)
            } else if trend != Trend::Flat && !can_lock_with_up && trend == Trend::UpRising && pnl_if_up_wins < pnl_if_down_wins && up_price_ok && wave_state.buys_up_since_lock < MAX_REBALANCE_BUYS {
                // Expansion: can't lock with Up, Up rising, PnL if Up wins worse → buy Up
                (true, false, false)
            } else if trend == Trend::UpRising
                && trend != Trend::Flat
                && up_price_ok
                && cost_per_pair_up <= REBALANCE_COST_PER_PAIR_MAX
                && wave_state.buys_up_since_lock < MAX_REBALANCE_BUYS
            {
                // Ride the winner (Example 4): Up is rising → buy Up to grow PnL if Up wins
                (true, false, false)
            } else if trend == Trend::DownRising
                && trend != Trend::Flat
                && down_price_ok
                && cost_per_pair_down <= REBALANCE_COST_PER_PAIR_MAX
                && wave_state.buys_down_since_lock < MAX_REBALANCE_BUYS
            {
                // Ride the winner: Down is rising → buy Down to grow PnL if Down wins
                (false, true, false)
            } else if trend != Trend::Flat
                && pnl_if_down_wins < 0.0
                && pnl_if_down_wins < pnl_if_up_wins
                && trend != Trend::UpRising
                && down_price_ok
                && cost_per_pair_down <= REBALANCE_COST_PER_PAIR_MAX
                && wave_state.buys_down_since_lock < MAX_REBALANCE_BUYS
            {
                // PnL rebalance: Down outcome negative and not riding Up → buy Down
                (false, true, false)
            } else if trend != Trend::Flat
                && pnl_if_up_wins < 0.0
                && pnl_if_up_wins < pnl_if_down_wins
                && trend != Trend::DownRising
                && up_price_ok
                && cost_per_pair_up <= REBALANCE_COST_PER_PAIR_MAX
                && wave_state.buys_up_since_lock < MAX_REBALANCE_BUYS
            {
                // PnL rebalance: Up outcome negative and not riding Down → buy Up
                (true, false, false)
            } else if trend == Trend::DownFalling && up_price_ok && wave_state.buys_up_since_lock < MAX_RISING_BUYS_PER_WAVE {
                // Example 6: no buy on Flat; only DownFalling (not Flat) → buy Up
                (true, false, false)
            } else if trend == Trend::UpFalling && down_price_ok && wave_state.buys_down_since_lock < MAX_RISING_BUYS_PER_WAVE {
                (false, true, false)
            } else {
                (false, false, false)
            }
        };

        if !do_buy_up && !do_buy_down {
            return Ok(());
        }

        let cooldown_secs = if snapshot.market_duration_secs >= 3600
            || snapshot.market_name.to_uppercase().contains("1H")
        {
            self.cooldown_seconds_1h
        } else {
            self.cooldown_seconds
        };
        let mut last = self.last_buy.lock().await;
        if let Some((ts, period)) = last.get(condition_id) {
            if *period == period_timestamp && current_time < ts + cooldown_secs {
                return Ok(());
            }
        }
        if last.get(condition_id).map(|(_, p)| *p) != Some(period_timestamp) {
            last.clear();
        }
        last.insert(condition_id.clone(), (current_time, period_timestamp));
        drop(last);

        let up_token_id = market_data.up_token.as_ref().map(|t| t.token_id.clone());
        let down_token_id = market_data.down_token.as_ref().map(|t| t.token_id.clone());

        if do_buy_up {
            let cost_pp = if pairs_after_up > 0.0 { cost_per_pair_up } else { up_ask };
            crate::log_println!(
                "📈 {}: buy Up | ${:.4} x {:.2} | cost_per_pair {:.4} (max {:.2})",
                market_name, up_ask, size, cost_pp, self.cost_per_pair_max
            );
            let (up_shares_after, up_avg_after, down_shares_after, down_avg_after, invest_up, invest_down, total_invest, pnl_if_up_wins, pnl_if_down_wins) = (
                new_up,
                new_up_cost / new_up,
                down_shares,
                if down_shares > 0.0 { down_avg } else { 0.0 },
                new_up_cost,
                down_cost,
                new_up_cost + down_cost,
                new_up - (new_up_cost + down_cost),
                down_shares - (new_up_cost + down_cost),
            );
            crate::log_println!(
                "   Position: Up {:.2} @ ${:.4} (invest ${:.2}) | Down {:.2} @ ${:.4} (invest ${:.2}) | total ${:.2} | PnL if Up wins ${:.2} | if Down wins ${:.2}",
                up_shares_after, up_avg_after, invest_up,
                down_shares_after, down_avg_after, invest_down,
                total_invest, pnl_if_up_wins, pnl_if_down_wins
            );
            if self.simulation_mode {
                self.record_trade(condition_id, period_timestamp, duration_secs, "Up", up_token_id.as_deref().unwrap_or(""), size, up_ask).await?;
            } else if let Some(ref up_id) = up_token_id {
                self.execute_buy_fak(market_name, "Up", up_id, size, up_ask).await?;
                self.record_trade(condition_id, period_timestamp, duration_secs, "Up", up_id, size, up_ask).await?;
            }
        } else {
            let cost_pp = if pairs_after_down > 0.0 { cost_per_pair_down } else { down_ask };
            crate::log_println!(
                "📉 {}: buy Down | ${:.4} x {:.2} | cost_per_pair {:.4} (max {:.2})",
                market_name, down_ask, size, cost_pp, self.cost_per_pair_max
            );
            let (up_shares_after, up_avg_after, down_shares_after, down_avg_after, invest_up, invest_down, total_invest, pnl_if_up_wins, pnl_if_down_wins) = (
                up_shares,
                if up_shares > 0.0 { up_avg } else { 0.0 },
                new_down,
                new_down_cost / new_down,
                up_cost,
                new_down_cost,
                up_cost + new_down_cost,
                up_shares - (up_cost + new_down_cost),
                new_down - (up_cost + new_down_cost),
            );
            crate::log_println!(
                "   Position: Up {:.2} @ ${:.4} (invest ${:.2}) | Down {:.2} @ ${:.4} (invest ${:.2}) | total ${:.2} | PnL if Up wins ${:.2} | if Down wins ${:.2}",
                up_shares_after, up_avg_after, invest_up,
                down_shares_after, down_avg_after, invest_down,
                total_invest, pnl_if_up_wins, pnl_if_down_wins
            );
            if self.simulation_mode {
                self.record_trade(condition_id, period_timestamp, duration_secs, "Down", down_token_id.as_deref().unwrap_or(""), size, down_ask).await?;
            } else if let Some(ref down_id) = down_token_id {
                self.execute_buy_fak(market_name, "Down", down_id, size, down_ask).await?;
                self.record_trade(condition_id, period_timestamp, duration_secs, "Down", down_id, size, down_ask).await?;
            }
        }

        // Update wave state: reset on lock, else increment side or flat_buys
        {
            let mut wave = self.wave_state.lock().await;
            let state = wave.entry(market_key.clone()).or_default();
            if is_lock {
                state.buys_up_since_lock = 0;
                state.buys_down_since_lock = 0;
                state.flat_buys_since_lock = 0;
            } else {
                let was_no_position = up_shares == 0.0 && down_shares == 0.0;
                let is_flat_trend = trend == Trend::Flat || trend == Trend::UpFalling || trend == Trend::DownFalling;
                if do_buy_up {
                    state.buys_up_since_lock = (state.buys_up_since_lock + 1).min(MAX_RISING_BUYS_PER_WAVE);
                    if was_no_position && is_flat_trend {
                        state.flat_buys_since_lock = (state.flat_buys_since_lock + 1).min(MAX_FLAT_BUYS_NO_POSITION);
                    }
                }
                if do_buy_down {
                    state.buys_down_since_lock = (state.buys_down_since_lock + 1).min(MAX_RISING_BUYS_PER_WAVE);
                    if was_no_position && is_flat_trend {
                        state.flat_buys_since_lock = (state.flat_buys_since_lock + 1).min(MAX_FLAT_BUYS_NO_POSITION);
                    }
                }
            }
        }

        Ok(())
    }

    async fn execute_buy_fak(
        &self,
        market_name: &str,
        side: &str,
        token_id: &str,
        shares: f64,
        price: f64,
    ) -> Result<()> {
        crate::log_println!(
            "{} BUY {} {:.2} shares @ ${:.4} (FAK - partial fill possible)",
            market_name, side, shares, price
        );
        let shares_rounded = (shares * 10000.0).round() / 10000.0;
        match self
            .api
            .place_market_order(token_id, shares_rounded, "BUY", Some("FAK"))
            .await
        {
            Ok(_) => crate::log_println!("REAL: FAK order placed"),
            Err(e) => {
                warn!("Failed to place FAK order: {}", e);
                return Err(e.into());
            }
        }
        Ok(())
    }

    async fn record_trade(
        &self,
        condition_id: &str,
        period_timestamp: u64,
        market_duration_secs: u64,
        side: &str,
        token_id: &str,
        shares: f64,
        price: f64,
    ) -> Result<()> {
        let market_key = format!("{}:{}", condition_id, period_timestamp);
        let mut trades = self.trades.lock().await;
        let trade = trades.entry(market_key.clone()).or_insert_with(|| CycleTrade {
            condition_id: condition_id.to_string(),
            period_timestamp,
            market_duration_secs,
            up_token_id: None,
            down_token_id: None,
            up_shares: 0.0,
            down_shares: 0.0,
            up_avg_price: 0.0,
            down_avg_price: 0.0,
        });
        match side {
            "Up" => {
                let old = trade.up_shares * trade.up_avg_price;
                trade.up_shares += shares;
                trade.up_avg_price = if trade.up_shares > 0.0 {
                    (old + shares * price) / trade.up_shares
                } else {
                    price
                };
                trade.up_token_id = Some(token_id.to_string());
            }
            "Down" => {
                let old = trade.down_shares * trade.down_avg_price;
                trade.down_shares += shares;
                trade.down_avg_price = if trade.down_shares > 0.0 {
                    (old + shares * price) / trade.down_shares
                } else {
                    price
                };
                trade.down_token_id = Some(token_id.to_string());
            }
            _ => {}
        }
        Ok(())
    }

    /// Check closed markets and compute PnL from the actual winning token (after resolution).
    /// In simulation this is the only place PnL is calculated; same logic in production.
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
            crate::log_println!("=== Market resolved ===");
            crate::log_println!(
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

            if !self.simulation_mode && (up_wins || down_wins) {
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
            crate::log_println!(
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

    pub async fn reset_period(&self) {
        let mut last = self.last_buy.lock().await;
        last.clear();
        let mut c = self.closure_checked.lock().await;
        c.clear();
        crate::log_println!("Period reset");
    }

    pub async fn get_total_profit(&self) -> f64 {
        *self.total_profit.lock().await
    }

    pub async fn get_period_profit(&self) -> f64 {
        *self.period_profit.lock().await
    }
}

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about = "Polymarket trading bot")]
pub struct Args {
    #[arg(short, long, default_value_t = true)]
    pub simulation: bool,

    #[arg(long)]
    pub production: bool,

    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    #[arg(long)]
    pub redeem: bool,

    #[arg(long, requires = "redeem")]
    pub condition_id: Option<String>,
}

impl Args {
    pub fn is_simulation(&self) -> bool {
        if self.production {
            false
        } else {
            self.simulation
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub trading: TradingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolymarketConfig {
    pub gamma_api_url: String,
    pub clob_api_url: String,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
    pub private_key: Option<String>,
    pub proxy_wallet_address: Option<String>,
    pub signature_type: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub check_interval_ms: u64,
    #[serde(default = "default_market_closure_check_interval")]
    pub market_closure_check_interval_seconds: u64,
    #[serde(default = "default_data_source")]
    pub data_source: String,
    #[serde(default = "default_markets")]
    pub markets: Vec<String>,
    /// Timeframes to trade: ["15m", "1h"]. Both 15m and 1h markets run when both are set.
    #[serde(default = "default_timeframes")]
    pub timeframes: Vec<String>,
    /// Max cost per pair when adding a side (target-like balancing). Add Up or Down only if (total_cost after add) / pairs <= this. e.g. 1.0 or 1.01.
    #[serde(default = "default_cost_per_pair_max")]
    pub cost_per_pair_max: f64,
    /// Never buy a token if its ask price is below this (e.g. 0.05). Avoids nearly-resolved markets where the other side has effectively won.
    #[serde(default = "default_min_side_price")]
    pub min_side_price: f64,
    /// Never buy a token if its ask price is above this (e.g. 0.99). Avoids overpaying for a near-certain outcome.
    #[serde(default = "default_max_side_price")]
    pub max_side_price: f64,
    /// Min seconds between buys (any side) per market. 0 = condition-based only. Set e.g. 10 for 15m.
    #[serde(default = "default_cooldown_seconds")]
    pub cooldown_seconds: u64,
    /// Min seconds between buys for 1h markets only (throttle 1h trading). Default 45.
    #[serde(default = "default_cooldown_seconds_1h")]
    pub cooldown_seconds_1h: u64,
    /// Shares per side; if unset/0, use per-market defaults (BTC 15m=24, ETH 15m=14).
    pub shares: Option<f64>,
    /// Reduce order size in the last N seconds (volatility). Default 300 (5 min).
    #[serde(default = "default_size_reduce_after_secs")]
    pub size_reduce_after_secs: u64,
    /// When reducing: size = base * (min_ratio + (1-min_ratio)*time_left/reduce_window). Default 0.5.
    #[serde(default = "default_size_min_ratio")]
    pub size_min_ratio: f64,
    /// Minimum shares per order when reducing. Default 5.
    #[serde(default = "default_size_min_shares")]
    pub size_min_shares: f64,
}

fn default_market_closure_check_interval() -> u64 {
    20
}

fn default_data_source() -> String {
    "api".to_string()
}

fn default_markets() -> Vec<String> {
    vec!["btc".to_string()]
}

fn default_timeframes() -> Vec<String> {
    vec!["15m".to_string(), "1h".to_string()]
}

fn default_cost_per_pair_max() -> f64 {
    1.01
}

fn default_min_side_price() -> f64 {
    0.05
}

fn default_max_side_price() -> f64 {
    0.99
}

fn default_cooldown_seconds() -> u64 {
    0
}

fn default_cooldown_seconds_1h() -> u64 {
    45
}

fn default_size_reduce_after_secs() -> u64 {
    300
}

fn default_size_min_ratio() -> f64 {
    0.5
}

fn default_size_min_shares() -> f64 {
    5.0
}

impl Default for Config {
    fn default() -> Self {
        Self {
            polymarket: PolymarketConfig {
                gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
                clob_api_url: "https://clob.polymarket.com".to_string(),
                api_key: None,
                api_secret: None,
                api_passphrase: None,
                private_key: None,
                proxy_wallet_address: None,
                signature_type: None,
            },
            trading: TradingConfig {
                check_interval_ms: 1000,
                market_closure_check_interval_seconds: 20,
                data_source: "api".to_string(),
                markets: vec!["btc".to_string()],
                timeframes: default_timeframes(),
                cost_per_pair_max: default_cost_per_pair_max(),
                min_side_price: default_min_side_price(),
                max_side_price: default_max_side_price(),
                cooldown_seconds: default_cooldown_seconds(),
                cooldown_seconds_1h: default_cooldown_seconds_1h(),
                shares: None,
                size_reduce_after_secs: default_size_reduce_after_secs(),
                size_min_ratio: default_size_min_ratio(),
                size_min_shares: default_size_min_shares(),
            },
        }
    }
}

impl Config {
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&content)?)
        } else {
            let config = Config::default();
            let content = serde_json::to_string_pretty(&config)?;
            std::fs::write(path, content)?;
            Ok(config)
        }
    }
}

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,

    #[arg(long)]
    pub redeem: bool,

    #[arg(long, requires = "redeem")]
    pub condition_id: Option<String>,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub strategy: StrategyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub price_limit: f64,
    pub shares: f64,
    pub place_order_before_mins: u64,
    pub sell_unmatched_after_mins: u64,
    pub check_interval_ms: u64,
    #[serde(default)]
    pub simulation_mode: bool,
    #[serde(default)]
    pub signal: SignalConfig,
    #[serde(default = "default_sell_opposite_above")]
    pub sell_opposite_above: f64,
    #[serde(default = "default_sell_opposite_time_remaining")]
    pub sell_opposite_time_remaining: u64,
    #[serde(default = "default_market_closure_check_interval_seconds")]
    pub market_closure_check_interval_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignalConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_stable_min")]
    pub stable_min: f64,
    #[serde(default = "default_stable_max")]
    pub stable_max: f64,
    #[serde(default = "default_clear_threshold")]
    pub clear_threshold: f64,
    #[serde(default = "default_clear_remaining_mins")]
    pub clear_remaining_mins: u64,
    #[serde(default = "default_danger_price")]
    pub danger_price: f64,
    #[serde(default = "default_danger_time_passed")]
    pub danger_time_passed: u64,
    #[serde(default = "default_one_side_buy_risk_management")]
    pub one_side_buy_risk_management: String,
    #[serde(default = "default_true")]
    pub mid_market_enabled: bool,
}

fn default_true() -> bool { true }
fn default_stable_min() -> f64 { 0.35 }
fn default_stable_max() -> f64 { 0.65 }
fn default_clear_threshold() -> f64 { 0.99 }
fn default_clear_remaining_mins() -> u64 { 15 }
fn default_danger_price() -> f64 { 0.15 }
fn default_danger_time_passed() -> u64 { 30 }
fn default_one_side_buy_risk_management() -> String { "price".to_string() }
fn default_sell_opposite_above() -> f64 { 0.95 }
fn default_sell_opposite_time_remaining() -> u64 { 15 }
fn default_market_closure_check_interval_seconds() -> u64 { 120 }

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
            strategy: StrategyConfig {
                price_limit: 0.45,
                shares: 5.0,
                place_order_before_mins: 3,
                sell_unmatched_after_mins: 57,
                check_interval_ms: 2000,
                simulation_mode: false,
                signal: SignalConfig::default(),
                sell_opposite_above: 0.95,
                sell_opposite_time_remaining: 15,
                market_closure_check_interval_seconds: 120,
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

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Configuration file path
    #[arg(short, long, default_value = "config.json")]
    pub config: PathBuf,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub polymarket: PolymarketConfig,
    pub strategy: StrategyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub price_limit: f64,             // $0.45
    pub shares: f64,                  // 5 shares
    pub place_order_before_mins: u64, // 3 mins
    pub sell_unmatched_after_mins: u64, // 5 mins - sell unmatched positions after this time
    pub check_interval_ms: u64,       // e.g. 5000
    /// Simulation mode: If true, bot will log actions but not place real orders
    #[serde(default)]
    pub simulation_mode: bool,
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
                sell_unmatched_after_mins: 5,
                check_interval_ms: 2000,
                simulation_mode: false,
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


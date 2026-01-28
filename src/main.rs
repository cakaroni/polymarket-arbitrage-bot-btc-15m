mod api;
mod config;
mod models;
mod discovery;
mod strategy;
mod websocket;

use anyhow::Result;
use clap::Parser;
use config::{Args, Config};
use std::io::Write;
use std::sync::Arc;
use api::PolymarketApi;
use strategy::PreLimitStrategy;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logger with custom format (no prefix)
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .format(|buf, record| {
            // Only show the message, no timestamp/level/module prefix
            writeln!(buf, "{}", record.args())
        })
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;

    eprintln!("🚀 Starting Polymarket Pre-Limit Order Bot");
    if config.strategy.simulation_mode {
        eprintln!("🎮 SIMULATION MODE ENABLED - No real orders will be placed");
        eprintln!("   Orders will match when prices hit ${:.2} or below", config.strategy.price_limit);
    }
    eprintln!("📈 Strategy: Placing Up/Down limit orders at ${:.2} for 4 markets", config.strategy.price_limit);

    // Initialize API client
    let api = Arc::new(PolymarketApi::new(
        config.polymarket.gamma_api_url.clone(),
        config.polymarket.clob_api_url.clone(),
        config.polymarket.api_key.clone(),
        config.polymarket.api_secret.clone(),
        config.polymarket.api_passphrase.clone(),
        config.polymarket.private_key.clone(),
        config.polymarket.proxy_wallet_address.clone(),
        config.polymarket.signature_type,
    ));

    // Authenticate with CLOB API
    if config.polymarket.private_key.is_some() {
        if let Err(e) = api.authenticate().await {
            log::error!("Authentication failed: {}", e);
            anyhow::bail!("Authentication failed. Please check your credentials.");
        }
    } else {
        log::warn!("⚠️ No private key provided. Bot will only be able to monitor markets.");
    }

    // Initialize and run strategy
    let strategy = PreLimitStrategy::new(api, config);
    strategy.run().await?;

    Ok(())
}

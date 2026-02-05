mod api;
mod config;
mod models;
mod monitor;
mod trader;

use anyhow::{Context, Result};
use chrono::{Datelike, TimeZone, Timelike};
use chrono_tz::America::New_York;
use clap::Parser;
use config::{Args, Config};
use log::warn;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::fs::{File, OpenOptions};

use api::PolymarketApi;
use monitor::MarketMonitor;
use trader::Trader;

struct DualWriter {
    stderr: io::Stderr,
    file: Mutex<File>,
}

impl Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.stderr.write_all(buf);
        let _ = self.stderr.flush();
        let mut file = self.file.lock().unwrap();
        file.write_all(buf)?;
        file.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        let mut file = self.file.lock().unwrap();
        file.flush()?;
        Ok(())
    }
}

unsafe impl Send for DualWriter {}
unsafe impl Sync for DualWriter {}

static HISTORY_FILE: OnceLock<Mutex<File>> = OnceLock::new();

fn init_history_file(file: File) {
    HISTORY_FILE.set(Mutex::new(file)).expect("History file already initialized");
}

pub fn log_to_history(message: &str) {
    eprint!("{}", message);
    let _ = io::stderr().flush();
    if let Some(file_mutex) = HISTORY_FILE.get() {
        if let Ok(mut file) = file_mutex.lock() {
            let _ = write!(file, "{}", message);
            let _ = file.flush();
        }
    }
}

#[macro_export]
macro_rules! log_println {
    ($($arg:tt)*) => {
        {
            let message = format!($($arg)*);
            $crate::log_to_history(&format!("{}\n", message));
        }
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    let history_path = "history.toml";
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
        .context("Failed to open history.toml for logging")?;

    let log_file_for_writer = OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
        .context("Failed to open history.toml for writer")?;

    init_history_file(log_file);

    let dual_writer = DualWriter {
        stderr: io::stderr(),
        file: Mutex::new(log_file_for_writer),
    };

    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .target(env_logger::Target::Pipe(Box::new(dual_writer)))
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config)?;

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

    if args.redeem {
        run_redeem_only(api.as_ref(), &config, args.condition_id.as_deref()).await?;
        return Ok(());
    }

    let is_simulation = args.is_simulation();
    eprintln!("Starting Polymarket Trading Bot");
    eprintln!("Mode: {}", if is_simulation { "SIMULATION" } else { "PRODUCTION" });
    if is_simulation {
        eprintln!("PnL will be calculated after each market closes and winner is known.");
    }

    if !is_simulation {
        match api.authenticate().await {
            Ok(_) => eprintln!("Authentication successful"),
            Err(e) => {
                warn!("Failed to authenticate: {}", e);
                warn!("Order placement may fail. Verify credentials in config.json");
            }
        }
    }

    let markets = &config.trading.markets;
    if markets.is_empty() {
        anyhow::bail!("No markets configured. Add markets to config (e.g. [\"btc\", \"eth\"])");
    }

    let cost_per_pair_max = config.trading.cost_per_pair_max;
    let min_side_price = config.trading.min_side_price;
    let max_side_price = config.trading.max_side_price;
    let cooldown = config.trading.cooldown_seconds;
    let cooldown_1h = config.trading.cooldown_seconds_1h;
    let shares_override = config.trading.shares;
    let size_reduce_after_secs = config.trading.size_reduce_after_secs;
    let size_min_ratio = config.trading.size_min_ratio;
    let size_min_shares = config.trading.size_min_shares;
    let data_source = config.trading.data_source.clone();

    let timeframes = &config.trading.timeframes;
    let timeframes_str: Vec<&str> = timeframes.iter().map(|s| s.as_str()).collect();
    eprintln!("Strategy: balance-aware, buy one side when cost per pair <= max");
    eprintln!("   Markets: {}", markets.join(", ").to_uppercase());
    eprintln!("   Timeframes: {}", timeframes_str.join(", "));
    eprintln!("   Cost per pair max: {}", cost_per_pair_max);
    eprintln!("   Min side price: ${:.2} (no buy below)", min_side_price);
    eprintln!("   Max side price: ${:.2} (no buy above)", max_side_price);
    eprintln!("   Cooldown: {}s (1h: {}s)", cooldown, cooldown_1h);
    eprintln!("   Shares: {:?} (default per market: BTC 15m=24, ETH 15m=14)", shares_override);
    eprintln!("   Order type: FAK (partial fills possible)");
    eprintln!("   Data source: {}", data_source.to_uppercase());
    eprintln!();

    let trader = Arc::new(Trader::new(
        api.clone(),
        is_simulation,
        cost_per_pair_max,
        min_side_price,
        max_side_price,
        cooldown,
        cooldown_1h,
        shares_override,
        size_reduce_after_secs,
        size_min_ratio,
        size_min_shares,
    ));
    let trader_closure = trader.clone();
    let market_closure_interval = config.trading.market_closure_check_interval_seconds;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(market_closure_interval));
        loop {
            interval.tick().await;
            if let Err(e) = trader_closure.check_market_closure().await {
                warn!("Error checking market closure: {}", e);
            }
            let total_profit = trader_closure.get_total_profit().await;
            let period_profit = trader_closure.get_period_profit().await;
            if total_profit != 0.0 || period_profit != 0.0 {
                crate::log_println!("Current Profit - Period: ${:.2} | Total: ${:.2}", period_profit, total_profit);
            }
        }
    });

    let mut handles = Vec::new();
    for asset in markets {
        for timeframe in timeframes {
            let asset_upper = asset.to_uppercase();
            let tf = timeframe.trim().to_lowercase();
            let market_name = format!("{} {}", asset_upper, timeframe);
            let duration_minutes = if tf == "1h" { 60 } else { 15 };
            let period_secs: u64 = if tf == "1h" { 3600 } else { 900 };

            eprintln!("Discovering {} market...", market_name);
            let market = match discover_market_for_asset_timeframe(&api, asset, duration_minutes).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to discover {} market: {}. Skipping...", market_name, e);
                    continue;
                }
            };

            let monitor = MarketMonitor::new(
                api.clone(),
                market_name.clone(),
                market,
                config.trading.check_interval_ms,
                data_source.clone(),
                config.polymarket.clob_api_url.clone(),
            );
            let monitor_arc = Arc::new(monitor);

            let monitor_for_period_check = monitor_arc.clone();
            let api_for_period_check = api.clone();
            let trader_for_period_reset = trader.clone();
            let asset_owned = asset.to_string();
            let market_name_owned = market_name.clone();
            let timeframe_owned = timeframe.clone();

            let handle = tokio::spawn(async move {
                let mut last_processed_period: Option<u64> = None;
                loop {
                    let current_market_timestamp = monitor_for_period_check.get_current_market_timestamp().await;
                    let next_period_timestamp = current_market_timestamp + period_secs;
                    let current_time = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let sleep_duration = if next_period_timestamp > current_time {
                        next_period_timestamp - current_time
                    } else {
                        0
                    };
                    tokio::time::sleep(tokio::time::Duration::from_secs(sleep_duration)).await;
                    let current_time = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let current_period = (current_time / period_secs) * period_secs;
                    if let Some(last_period) = last_processed_period {
                        if current_period == last_period {
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                    eprintln!("New period detected for {}! (Period: {}) Discovering new market...", market_name_owned, current_period);
                    last_processed_period = Some(current_period);
                    let duration_min = if timeframe_owned.trim().eq_ignore_ascii_case("1h") { 60 } else { 15 };
                    match discover_market_for_asset_timeframe(&api_for_period_check, &asset_owned, duration_min).await {
                        Ok(new_market) => {
                            if let Err(e) = monitor_for_period_check.update_market(new_market).await {
                                warn!("Failed to update {} market: {}", market_name_owned, e);
                            } else {
                                trader_for_period_reset.reset_period().await;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to discover new {} market: {}", market_name_owned, e);
                            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        }
                    }
                }
            });
            handles.push(handle);

            let monitor_start = monitor_arc.clone();
            let trader_start = trader.clone();
            tokio::spawn(async move {
                monitor_start
                    .start_monitoring(move |snapshot| {
                        let trader = trader_start.clone();
                        async move {
                            if let Err(e) = trader.process_snapshot(&snapshot).await {
                                warn!("Error processing snapshot: {}", e);
                            }
                        }
                    })
                    .await;
            });
        }
    }

    if handles.is_empty() {
        anyhow::bail!("No valid markets found. Check your market configuration.");
    }

    eprintln!("Started monitoring {} market(s)", handles.len());
    futures::future::join_all(handles).await;
    Ok(())
}

async fn run_redeem_only(
    api: &PolymarketApi,
    config: &Config,
    condition_id: Option<&str>,
) -> Result<()> {
    let proxy = config
        .polymarket
        .proxy_wallet_address
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--redeem requires proxy_wallet_address in config.json"))?;

    eprintln!("Redeem-only mode (proxy: {})", proxy);
    let cids: Vec<String> = if let Some(cid) = condition_id {
        let cid = if cid.starts_with("0x") { cid.to_string() } else { format!("0x{}", cid) };
        eprintln!("Redeeming condition: {}", cid);
        vec![cid]
    } else {
        eprintln!("Fetching redeemable positions...");
        let list = api.get_redeemable_positions(proxy).await?;
        if list.is_empty() {
            eprintln!("No redeemable positions found.");
            return Ok(());
        }
        eprintln!("Found {} condition(s) to redeem.", list.len());
        list
    };

    let mut ok_count = 0u32;
    let mut fail_count = 0u32;
    for cid in &cids {
        eprintln!("\n--- Redeeming condition {} ---", &cid[..cid.len().min(18)]);
        match api.redeem_tokens(cid, "", "Up").await {
            Ok(_) => {
                eprintln!("Success: {}", cid);
                ok_count += 1;
            }
            Err(e) => {
                eprintln!("Failed to redeem {}: {} (skipping)", cid, e);
                fail_count += 1;
            }
        }
    }
    eprintln!("\nRedeem complete. Succeeded: {}, Failed: {}", ok_count, fail_count);
    Ok(())
}

async fn discover_market_for_asset_timeframe(
    api: &PolymarketApi,
    asset: &str,
    market_duration_minutes: u64,
) -> Result<crate::models::Market> {
    let current_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut seen_ids = std::collections::HashSet::new();
    let asset_lower = asset.to_lowercase();
    let slug_prefix = match asset_lower.as_str() {
        "btc" => "btc",
        "eth" => "eth",
        "sol" => "sol",
        "xrp" => "xrp",
        _ => anyhow::bail!("Unsupported asset: {}. Supported: BTC, ETH, SOL, XRP", asset),
    };
    let timeframe_str = if market_duration_minutes == 60 { "1h" } else { "15m" };
    discover_market(api, asset, slug_prefix, market_duration_minutes, current_time, &mut seen_ids).await
        .context(format!("Failed to discover {} {} market", asset, timeframe_str))
}

/// Build 1h market slug in Polymarket format: bitcoin-up-or-down-february-2-11pm-et
fn slug_1h_human_readable(period_start_unix: u64, slug_prefix: &str) -> String {
    let dt_utc = chrono::Utc.timestamp_opt(period_start_unix as i64, 0).single().unwrap();
    let dt_et = dt_utc.with_timezone(&New_York);
    let month = match dt_et.month() {
        1 => "january",
        2 => "february",
        3 => "march",
        4 => "april",
        5 => "may",
        6 => "june",
        7 => "july",
        8 => "august",
        9 => "september",
        10 => "october",
        11 => "november",
        12 => "december",
        _ => "january",
    };
    let day = dt_et.day();
    let hour_24 = dt_et.hour();
    let (hour_12, am_pm) = if hour_24 == 0 {
        (12, "am")
    } else if hour_24 < 12 {
        (hour_24, "am")
    } else if hour_24 == 12 {
        (12, "pm")
    } else {
        (hour_24 - 12, "pm")
    };
    let asset_name = match slug_prefix {
        "btc" => "bitcoin",
        "eth" => "ethereum",
        _ => slug_prefix,
    };
    format!(
        "{}-up-or-down-{}-{}-{}{}-et",
        asset_name, month, day, hour_12, am_pm
    )
}

async fn discover_market(
    api: &PolymarketApi,
    market_name: &str,
    slug_prefix: &str,
    market_duration_minutes: u64,
    current_time: u64,
    seen_ids: &mut std::collections::HashSet<String>,
) -> Result<crate::models::Market> {
    let (period_duration_secs, timeframe_str) = if market_duration_minutes == 60 {
        (3600u64, "1h")
    } else if market_duration_minutes == 15 {
        (900u64, "15m")
    } else {
        anyhow::bail!("Only 15m and 1h markets are supported, got {}m", market_duration_minutes);
    };
    let rounded_time = (current_time / period_duration_secs) * period_duration_secs;

    let slug = if market_duration_minutes == 60 {
        slug_1h_human_readable(rounded_time, slug_prefix)
    } else {
        format!("{}-updown-15m-{}", slug_prefix, rounded_time)
    };

    if let Ok(market) = api.get_market_by_slug(&slug).await {
        if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
            eprintln!(
                "Found {} {} market by slug: {} | Condition ID: {}",
                market_name, timeframe_str, market.slug, market.condition_id
            );
            return Ok(market);
        }
    }

    for offset in 1..=3 {
        let try_time = rounded_time - (offset * period_duration_secs);
        let try_slug = if market_duration_minutes == 60 {
            slug_1h_human_readable(try_time, slug_prefix)
        } else {
            format!("{}-updown-15m-{}", slug_prefix, try_time)
        };
        eprintln!("Trying previous {} {} market by slug: {}", market_name, timeframe_str, try_slug);
        if let Ok(market) = api.get_market_by_slug(&try_slug).await {
            if !seen_ids.contains(&market.condition_id) && market.active && !market.closed {
                eprintln!(
                    "Found {} {} market by slug: {} | Condition ID: {}",
                    market_name, timeframe_str, market.slug, market.condition_id
                );
                return Ok(market);
            }
        }
    }

    anyhow::bail!(
        "Could not find active {} {} up/down market. Set condition_id in config if needed.",
        market_name,
        timeframe_str
    )
}

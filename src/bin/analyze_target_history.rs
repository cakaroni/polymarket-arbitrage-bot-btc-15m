//! Parses target trading history files (btc-15m.toml, eth-15m.toml, btc-1h.toml, eth-1h.toml),
//! sorts trades chronologically, and prints:
//! - Total cost, shares Up/Down, PnL if Up wins, PnL if Down wins, realized PnL
//! - Side switch counts and approximate intervals (time between buying opposite side).
//!
//! Run: cargo run --bin analyze_target_history [-- path/to/dir]
//! Default dir: current directory (looks for btc-15m.toml, etc.)

use std::fs;
use std::path::Path;

fn parse_line(line: &str) -> Option<(i64, String, f64, f64)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('>') || line.starts_with("Market") || line.starts_with("Condition") || line.starts_with("Time") || line.starts_with('-') {
        return None;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    // 0: date 1: time (or datetime in one token) 2: BUY 3: Up|Down 4: price 5: size
    let (time_str, side, price_str, size_str) = if parts[1] == "BUY" {
        (parts[0], parts[3], parts[4], parts[5])
    } else if parts.len() >= 6 && (parts[2] == "Up" || parts[2] == "Down") {
        (parts[0], parts[2], parts[3], parts[4])
    } else {
        (parts[0], parts[3], parts[4], parts[5])
    };
    if side != "Up" && side != "Down" {
        return None;
    }
    let price: f64 = price_str.parse().ok()?;
    let size: f64 = size_str.parse().ok()?;
    // Parse ISO timestamp to unix-like for sorting (we only need ordering)
    let time_secs = parse_iso_to_secs(time_str)?;
    Some((time_secs, side.to_string(), price, size))
}

fn parse_iso_to_secs(s: &str) -> Option<i64> {
    // 2026-02-03T06:12:28.000Z
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let (y, rest) = date.split_once('-')?;
    let (m, d) = rest.split_once('-')?;
    let (h, rest) = time.split_once(':')?;
    let (min, rest) = rest.split_once(':')?;
    let sec = rest.split('.').next().unwrap_or(rest);
    let y: i64 = y.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    let d: i64 = d.parse().ok()?;
    let h: i64 = h.parse().ok()?;
    let min: i64 = min.parse().ok()?;
    let sec: i64 = sec.parse().ok()?;
    Some((y - 1970) * 365 * 24 * 3600 + (m - 1) * 31 * 24 * 3600 + (d - 1) * 24 * 3600 + h * 3600 + min * 60 + sec)
}

fn extract_won(content: &str) -> &str {
    if content.contains("Won: Down token") || content.contains("Won: Down") {
        "Down"
    } else if content.contains("Won: Up token") || content.contains("Won: Up") {
        "Up"
    } else {
        "Unknown"
    }
}

fn analyze_file(path: &Path) -> Option<(String, String, Vec<(i64, String, f64, f64)>)> {
    let content = fs::read_to_string(path).ok()?;
    let won = extract_won(&content).to_string();
    let name = path.file_stem()?.to_str()?.to_string();
    let mut trades = Vec::new();
    for line in content.lines() {
        if let Some(t) = parse_line(line) {
            trades.push(t);
        }
    }
    trades.sort_by_key(|t| t.0);
    Some((name, won, trades))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).map(|s| s.as_str()).unwrap_or(".");
    let files = ["btc-15m.toml", "eth-15m.toml", "btc-1h.toml", "eth-1h.toml"];
    println!("Target trading history analysis (dir: {})\n", dir);
    for file in &files {
        let path = Path::new(dir).join(file);
        if !path.exists() {
            println!("Skip {} (not found)", file);
            continue;
        }
        let (name, won, trades) = match analyze_file(&path) {
            Some(x) => x,
            None => {
                println!("Skip {} (parse failed)", file);
                continue;
            }
        };
        let mut cost_up = 0.0;
        let mut cost_down = 0.0;
        let mut shares_up = 0.0;
        let mut shares_down = 0.0;
        let mut last_side: Option<String> = None;
        let mut switch_times = Vec::new();
        for (ts, side, price, size) in &trades {
            let cost = price * size;
            if side == "Up" {
                cost_up += cost;
                shares_up += size;
            } else {
                cost_down += cost;
                shares_down += size;
            }
            if let Some(ref last) = last_side {
                if last != side {
                    switch_times.push(*ts);
                }
            }
            last_side = Some(side.clone());
        }
        let total_cost = cost_up + cost_down;
        let pnl_if_up = shares_up * 1.0 - total_cost;
        let pnl_if_down = shares_down * 1.0 - total_cost;
        let realized = if won == "Up" { pnl_if_up } else { pnl_if_down };
        let switch_intervals: Vec<i64> = switch_times
            .windows(2)
            .map(|w| w[1] - w[0])
            .collect();
        let avg_interval_secs = if switch_intervals.is_empty() {
            None
        } else {
            Some(switch_intervals.iter().sum::<i64>() / switch_intervals.len() as i64)
        };
        println!("=== {} ===", name);
        println!("  Trades: {} (Up: {:.1} shares, Down: {:.1} shares)", trades.len(), shares_up, shares_down);
        println!("  Total cost: ${:.2}", total_cost);
        println!("  PnL if Up wins:   ${:.2}", pnl_if_up);
        println!("  PnL if Down wins: ${:.2}", pnl_if_down);
        println!("  Won: {} => Realized PnL: ${:.2}", won, realized);
        println!("  Side switches: {} | Avg interval between switches: {:?} sec", switch_times.len(), avg_interval_secs);
        println!();
    }
}

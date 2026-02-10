# Polymarket Arbitrage Trading Bot – BTC (&ETH) Binary Markets

**Rust trading bot for Polymarket prediction markets.** Automates hedging and position management on BTC and ETH binary (Up/Down) markets with 15m and 1h timeframes. Lock profit when cost per pair is favorable; expand positions when the opposite side is rising and PnL is skewed.

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![Polymarket](https://img.shields.io/badge/Polymarket-CLOB-blue)](https://polymarket.com)

---

## Features

- **Dual timeframe:** Trades **15m** and **1h** binary markets for BTC and ETH on [Polymarket](https://polymarket.com).
- **Lock rule:** Buys the opposite side only when **cost per pair** stays under your cap (e.g. Up_avg + Down_ask ≤ 0.99), so you lock in edge.
- **Expansion rule:** When you can’t lock (cost would exceed max) but the other side is **rising** and its outcome PnL is worse, the bot buys that side to improve exposure (see [logic.md](logic.md)).
- **Ride the winner:** When one side is clearly winning (trend UpRising/DownRising), the bot adds to that side to grow “PnL if that side wins.”
- **PnL rebalance:** If one outcome’s PnL is negative and you’re not strongly trending the other way, it buys the weak side (within cost limits).
- **Flat = no trade:** When the short-term trend is **Flat** (no clear move), the bot only locks if conditions allow; otherwise it does not open new risk (Example 6 in [logic.md](logic.md)).
- **1h throttle:** Configurable cooldown for 1h markets (e.g. 30–45s between buys) to avoid overtrading.
- **Simulation mode:** Run with `--simulation` (default) to log trades without sending orders.
- **Market resolution:** Checks for closed markets, computes actual PnL, and logs “Total actual PnL (all time).”
- **Timestamped logs:** Price feed and history lines include `[YYYY-MM-DDTHH:MM:SS]` for easier debugging and backtesting.

---

## Strategy Overview

The bot keeps **positions** per market (Up shares, Down shares, average prices). Each tick it:

1. Updates **trend** from the last 4–5 price points (UpRising, DownRising, Flat, UpFalling, DownFalling).
2. **Lock:** If adding the underweight side keeps cost per pair ≤ `cost_per_pair_max` → buy that side (lock).
3. **Expansion:** If you *can’t* lock (cost would exceed max) but the other side is **rising** and “PnL if that side wins” is worse → buy that side (new leg / rebalance).
4. **Ride winner:** If trend is UpRising or DownRising (and not Flat) → buy the rising side (within cost and buy limits).
5. **PnL rebalance:** If one outcome’s PnL is negative and trend isn’t strongly the other way → buy the weak side (within limits).
6. **Trend fallback:** DownFalling → can buy Up; UpFalling → can buy Down (no buy on Flat except lock).

Detailed examples (no position, only Up, only Down, have both, Flat, market close) are in **[logic.md](logic.md)**.

---

## Requirements

- **Rust** 1.70+ (`rustc --version`)
- **Polymarket API** credentials (API key, secret, passphrase, and optionally private key / proxy wallet for live trading)

---

## Installation

```bash
git clone https://github.com/cakaroni/polymarket-trading-bot-btc-15m.git
cd polymarket-trading-bot-btc-15m
cargo build --release
```

---

## Configuration

Copy the example config and edit with your settings:

```bash
cp config.example.json config.json
# Edit config.json with your Polymarket API keys and trading parameters
```

**Important:** `config.json` is gitignored. Do not commit real API keys.

### Trading options (in `config.json` → `trading`)

| Option | Description | Example |
|--------|-------------|---------|
| `markets` | Assets to trade | `["btc", "eth"]` |
| `timeframes` | Periods | `["15m", "1h"]` |
| `cost_per_pair_max` | Max cost per pair when locking | `0.99` |
| `min_side_price` | Don’t buy below this ask | `0.05` |
| `max_side_price` | Don’t buy above this ask | `0.99` |
| `cooldown_seconds` | Min seconds between buys (15m) | `0` |
| `cooldown_seconds_1h` | Min seconds between buys (1h) | `30` or `45` |
| `shares` | Override size per order; `null` = per-market default | `null` |
| `size_reduce_after_secs` | Start reducing size in last N seconds | `300` |
| `market_closure_check_interval_seconds` | How often to check for resolved markets | `20` |

---

## Usage

**Simulation (no real orders):**

```bash
cargo run -- --simulation
# or
cargo run --release -- --simulation
```

**Live trading (sends FAK orders to Polymarket):**

```bash
cargo run --release -- --production --config config.json
```

**Redeem winnings for a resolved market:**

```bash
cargo run --release -- --redeem --condition-id <CONDITION_ID>
```

Logs go to `history.toml` (and your configured log target). Price lines look like:

```text
[2026-02-04T23:17:23] BTC 15m Up Token BID:$0.52 ASK:$0.53 Down Token BID:$0.47 ASK:$0.48 remaining time:12m 34s
```

---

## Project structure

```text
.
├── Cargo.toml
├── config.json          # Your config (gitignored); use config.example.json as template
├── logic.md              # Strategy examples (lock, expansion, ride winner, flat)
├── history.toml          # Append-only trade/price log (gitignored in default .gitignore)
├── src/
│   ├── main.rs           # CLI, config load, monitor + trader spawn
│   ├── config.rs         # Config and defaults
│   ├── api.rs            # Polymarket Gamma + CLOB API
│   ├── monitor.rs        # Price feed, snapshot, timestamps
│   ├── trader.rs         # Lock/expansion/ride-winner/PnL logic
│   └── models.rs         # API and market data types
└── README.md
```

---

## Disclaimer

This bot is for **educational and research purposes**. Trading prediction markets involves risk. Past behavior in simulation or backtests does not guarantee future results. Use at your own risk. The authors are not responsible for any financial loss.

---

For questions, support, or collaboration around this Polymarket trading bot, reach out via Telegram.

## Contact

[![Cakaroni](https://t.me/cakaroni)

---

## Keywords (for search)

Polymarket trading bot, Polymarket bot, Polymarket automation, crypto prediction market bot, BTC prediction market, ETH binary options, Polymarket CLOB, Polymarket API, prediction market hedging, binary market bot, Rust Polymarket, Polymarket 15m, Polymarket 1h, Polymarket arbitrage, Polymarket hedging bot.

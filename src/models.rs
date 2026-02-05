use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    #[serde(rename = "conditionId")]
    pub condition_id: String,
    #[serde(rename = "id")]
    pub market_id: Option<String>,
    pub question: String,
    pub slug: String,
    #[serde(rename = "resolutionSource")]
    pub resolution_source: Option<String>,
    #[serde(rename = "endDateISO")]
    pub end_date_iso: Option<String>,
    #[serde(rename = "endDateIso")]
    pub end_date_iso_alt: Option<String>,
    pub active: bool,
    pub closed: bool,
    pub tokens: Option<Vec<Token>>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    pub outcomes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(rename = "tokenId")]
    pub token_id: String,
    pub outcome: String,
    pub price: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: Vec<OrderBookEntry>,
    pub asks: Vec<OrderBookEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookEntry {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone)]
pub struct TokenPrice {
    pub token_id: String,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

impl TokenPrice {
    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.bid, self.ask) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::from(2)),
            (Some(bid), None) => Some(bid),
            (None, Some(ask)) => Some(ask),
            (None, None) => None,
        }
    }

    pub fn ask_price(&self) -> Decimal {
        self.ask.unwrap_or(Decimal::ZERO)
    }
}

/// Order structure for creating orders (before signing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: String, // "BUY" or "SELL"
    pub size: String,
    pub price: String,
    #[serde(rename = "type")]
    pub order_type: String, // "LIMIT" or "MARKET"
}

/// Signed order structure for posting to Polymarket
/// According to Polymarket docs, orders must be signed with private key before posting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedOrder {
    #[serde(rename = "tokenID")]
    pub token_id: String,
    pub side: String,
    pub size: String,
    pub price: String,
    #[serde(rename = "type")]
    pub order_type: String, // "LIMIT" or "MARKET"
    pub signature: Option<String>,
    pub signer: Option<String>,
    pub nonce: Option<u64>,
    pub expiration: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    pub order_id: Option<String>,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceResponse {
    pub balance: String,
    pub allowance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemResponse {
    pub success: bool,
    pub message: Option<String>,
    pub transaction_hash: Option<String>,
    pub amount_redeemed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MarketData {
    pub condition_id: String,
    pub market_name: String,
    pub up_token: Option<TokenPrice>,
    pub down_token: Option<TokenPrice>,
}

#[derive(Debug, Clone)]
pub struct PendingTrade {
    pub token_id: String,
    pub condition_id: String,
    pub investment_amount: f64,
    pub total_units: f64,
    pub remaining_units: f64,
    pub purchase_price: f64,
    pub difference_d: f64,
    pub timestamp: std::time::Instant,
    pub market_timestamp: u64,
    pub sell_points: Vec<f64>,
    pub sell_percentages: Vec<f64>,
    pub next_sell_index: usize,
}

#[derive(Debug, Clone)]
pub struct ArbitrageTrade {
    pub up_token_id: String,
    pub up_token_name: String,
    pub up_investment_amount: f64,
    pub up_total_units: f64,
    pub up_remaining_units: f64,
    pub up_purchase_price: f64,
    pub up_price_history: Vec<(std::time::Instant, f64)>,
    pub down_token_id: String,
    pub down_token_name: String,
    pub down_investment_amount: f64,
    pub down_total_units: f64,
    pub down_remaining_units: f64,
    pub down_purchase_price: f64,
    pub down_price_history: Vec<(std::time::Instant, f64)>,
    pub condition_id: String,
    pub price_gap: f64,
    pub min_profit_target: f64,
    pub timestamp: std::time::Instant,
    pub market_timestamp: u64,
    pub market_duration: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketToken {
    pub outcome: String,
    pub price: rust_decimal::Decimal,
    #[serde(rename = "token_id")]
    pub token_id: String,
    pub winner: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDetails {
    #[serde(rename = "accepting_order_timestamp")]
    pub accepting_order_timestamp: Option<String>,
    #[serde(rename = "accepting_orders")]
    pub accepting_orders: bool,
    pub active: bool,
    pub archived: bool,
    pub closed: bool,
    #[serde(rename = "condition_id")]
    pub condition_id: String,
    pub description: String,
    #[serde(rename = "enable_order_book")]
    pub enable_order_book: bool,
    #[serde(rename = "end_date_iso")]
    pub end_date_iso: String,
    pub fpmm: String,
    #[serde(rename = "game_start_time")]
    pub game_start_time: Option<String>,
    pub icon: String,
    pub image: String,
    #[serde(rename = "is_50_50_outcome")]
    pub is_50_50_outcome: bool,
    #[serde(rename = "maker_base_fee")]
    pub maker_base_fee: rust_decimal::Decimal,
    #[serde(rename = "market_slug")]
    pub market_slug: String,
    #[serde(rename = "minimum_order_size")]
    pub minimum_order_size: rust_decimal::Decimal,
    #[serde(rename = "minimum_tick_size")]
    pub minimum_tick_size: rust_decimal::Decimal,
    #[serde(rename = "neg_risk")]
    pub neg_risk: bool,
    #[serde(rename = "neg_risk_market_id")]
    pub neg_risk_market_id: String,
    #[serde(rename = "neg_risk_request_id")]
    pub neg_risk_request_id: String,
    #[serde(rename = "notifications_enabled")]
    pub notifications_enabled: bool,
    pub question: String,
    #[serde(rename = "question_id")]
    pub question_id: String,
    pub rewards: Rewards,
    #[serde(rename = "seconds_delay")]
    pub seconds_delay: u32,
    pub tags: Vec<String>,
    #[serde(rename = "taker_base_fee")]
    pub taker_base_fee: rust_decimal::Decimal,
    pub tokens: Vec<MarketToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rewards {
    #[serde(rename = "max_spread")]
    pub max_spread: rust_decimal::Decimal,
    #[serde(rename = "min_size")]
    pub min_size: rust_decimal::Decimal,
    pub rates: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    #[serde(rename = "id")]
    pub id: Option<String>,
    #[serde(rename = "tokenID")]
    pub token_id: Option<String>,
    #[serde(rename = "asset")]
    pub asset: Option<String>,
    #[serde(rename = "tokenName")]
    pub token_name: Option<String>,
    pub side: String,
    #[serde(rename = "size")]
    pub size: f64,
    #[serde(rename = "usdcSize")]
    pub usdc_size: Option<f64>,
    #[serde(rename = "price")]
    pub price: f64,
    #[serde(rename = "timestamp")]
    pub timestamp: u64,
    #[serde(rename = "orderID")]
    pub order_id: Option<String>,
    #[serde(rename = "user")]
    pub user: Option<String>,
    #[serde(rename = "proxyWallet")]
    pub proxy_wallet: Option<String>, // Proxy wallet from Data API
    #[serde(rename = "maker")]
    pub maker: Option<String>,
    #[serde(rename = "taker")]
    pub taker: Option<String>,
    #[serde(rename = "fee")]
    pub fee: Option<String>,
    #[serde(rename = "conditionId")]
    pub condition_id: Option<String>, // Condition ID from Data API
    #[serde(rename = "outcomeIndex")]
    pub outcome_index: Option<u32>, // Outcome index (0=Up, 1=Down)
    #[serde(rename = "outcome")]
    pub outcome: Option<String>, // "Up" or "Down"
    #[serde(rename = "type")]
    pub activity_type: Option<String>, // "TRADE", "REDEMPTION", etc.
    #[serde(rename = "transactionHash")]
    pub transaction_hash: Option<String>,
    #[serde(rename = "title")]
    pub title: Option<String>, // Market title
    #[serde(rename = "slug")]
    pub slug: Option<String>, // Market slug
}

impl Fill {
    /// Get the token ID, trying multiple fields
    pub fn get_token_id(&self) -> Option<&String> {
        self.token_id.as_ref()
            .or_else(|| self.asset.as_ref())
    }
    
    /// Get the user address, trying multiple fields
    pub fn get_user_address(&self) -> Option<&String> {
        self.user.as_ref()
            .or_else(|| self.proxy_wallet.as_ref())
            .or_else(|| self.maker.as_ref())
            .or_else(|| self.taker.as_ref())
    }
}

/// Response from fills endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillsResponse {
    pub fills: Option<Vec<Fill>>,
    #[serde(flatten)]
    pub other: serde_json::Value,
}

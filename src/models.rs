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
    #[serde(rename = "endDateISO")]
    pub end_date_iso: Option<String>,
    pub active: bool,
    pub closed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketDetails {
    #[serde(rename = "condition_id")]
    pub condition_id: String,
    pub question: String,
    pub tokens: Vec<MarketToken>,
    pub active: bool,
    pub closed: bool,
    #[serde(rename = "end_date_iso")]
    pub end_date_iso: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketToken {
    pub outcome: String,
    #[serde(rename = "token_id")]
    pub token_id: String,
    pub winner: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: String,
    pub side: String,
    pub size: String,
    pub price: String,
    #[serde(rename = "type")]
    pub order_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResponse {
    pub order_id: Option<String>,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]

pub struct RedeemResponse {
    pub success: bool,
    pub message: Option<String>,
    pub transaction_hash: Option<String>,
    pub amount_redeemed: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PreLimitOrderState {
    pub asset: String,
    pub condition_id: String,
    pub up_token_id: String,
    pub down_token_id: String,
    pub up_order_id: Option<String>,
    pub down_order_id: Option<String>,
    pub up_order_price: f64,
    pub down_order_price: f64,
    pub up_matched: bool,
    pub down_matched: bool,
    pub merged: bool,
    pub expiry: i64,
    pub risk_sold: bool,
    pub order_placed_at: i64,
    pub market_period_start: i64,
    /// Timestamp when we first had only one side matched (for sell_after_danger_time_passed)
    pub one_side_matched_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPrice {
    pub token_id: String,
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    #[serde(rename = "tokenID")]
    pub token_id: Option<String>,
    pub side: String,
    pub size: f64,
    pub price: f64,
    pub timestamp: u64,
    #[serde(rename = "conditionId")]
    pub condition_id: Option<String>,
}

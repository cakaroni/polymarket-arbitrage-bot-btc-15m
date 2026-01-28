use crate::api::PolymarketApi;
use crate::models::Market;
use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;

pub struct MarketDiscovery {
    api: Arc<PolymarketApi>,
}

impl MarketDiscovery {
    pub fn new(api: Arc<PolymarketApi>) -> Self {
        Self { api }
    }

    /// Discover 15m markets for BTC, ETH, SOL, XRP
    pub async fn discover_15m_markets(&self) -> Result<Vec<(String, Market)>> {
        let assets = vec!["bitcoin", "ethereum", "solana", "ripple"];
        let asset_names = vec!["BTC", "ETH", "SOL", "XRP"];
        
        let current_time = Utc::now().timestamp() as u64;
        let period_start = (current_time / 900) * 900;
        
        let mut markets = Vec::new();
        
        for (idx, asset) in assets.iter().enumerate() {
            let asset_name = asset_names[idx];
            
            // Try current and next period
            for offset in 0..=1 {
                let timestamp = period_start + (offset * 900);
                let slug = format!("{}-updown-15m-{}", asset, timestamp);
                
                match self.api.get_market_by_slug(&slug).await {
                    Ok(market) => {
                        if market.active && !market.closed {
                            markets.push((asset_name.to_string(), market));
                            break; // Found one for this asset
                        }
                    }
                    Err(_) => {
                        // Try alternative slug format if needed
                    }
                }
            }
        }
        
        Ok(markets)
    }

    pub async fn get_market_tokens(&self, condition_id: &str) -> Result<(String, String)> {
        let details = self.api.get_market(condition_id).await?;
        let mut up_token = None;
        let mut down_token = None;
        
        for token in details.tokens {
            let outcome = token.outcome.to_uppercase();
            if outcome.contains("UP") || outcome == "1" {
                up_token = Some(token.token_id);
            } else if outcome.contains("DOWN") || outcome == "0" {
                down_token = Some(token.token_id);
            }
        }
        
        let up = up_token.ok_or_else(|| anyhow::anyhow!("Up token not found"))?;
        let down = down_token.ok_or_else(|| anyhow::anyhow!("Down token not found"))?;
        
        Ok((up, down))
    }
}

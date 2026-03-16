use crate::domain::models::Exchange;

/// 账户希腊值 (按币种)
#[derive(Debug, Clone)]
pub struct Greeks {
    pub exchange: Exchange,
    /// 币种 (e.g., "BTC", "ETH")
    pub ccy: String,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub timestamp: u64,
}

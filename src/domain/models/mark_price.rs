use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Price, Timestamp};

/// 标记价格
#[derive(Debug, Clone)]
pub struct MarkPrice {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub price: Price,
    pub timestamp: Timestamp,
}

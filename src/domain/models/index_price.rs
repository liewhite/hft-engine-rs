use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Price, Timestamp};

/// 指数价格
#[derive(Debug, Clone)]
pub struct IndexPrice {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub price: Price,
    pub timestamp: Timestamp,
}

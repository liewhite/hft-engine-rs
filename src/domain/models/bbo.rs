use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Price, Quantity, Timestamp};

/// Best Bid Offer (L1 行情)
#[derive(Debug, Clone)]
pub struct BBO {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub bid_price: Price,
    pub bid_qty: Quantity,
    pub ask_price: Price,
    pub ask_qty: Quantity,
    pub timestamp: Timestamp,
}

impl BBO {
    /// 计算买卖价差
    pub fn spread(&self) -> Price {
        self.ask_price - self.bid_price
    }

    /// 计算中间价
    pub fn mid_price(&self) -> Price {
        (self.bid_price + self.ask_price) / 2.0
    }
}

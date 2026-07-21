use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Price, Quantity, Timestamp};

/// 公共成交印记 (市场上的匿名成交，非本账户成交)。
///
/// `is_buyer_maker = true` 表示买方是挂单方 -> 本笔为主动卖出 (taker 卖)。
/// 它既是策略可见的市场信号，也是回测撮合的成交来源 (见 `sim::SimState::on_market` 的
/// trade-print 撮合：真实成交价严格越过挂单价即触发 maker 成交)。
#[derive(Debug, Clone)]
pub struct MarketTrade {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub price: Price,
    pub qty: Quantity,
    pub is_buyer_maker: bool,
    pub timestamp: Timestamp,
}

use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Price, Quantity, Timestamp};

/// 公共成交印记 (市场上的匿名成交，非本账户成交)。
///
/// **本类型是回测撮合的成交来源**：`sim::SimState::on_market` 的 trade-print 撮合按
/// `price` 与 `qty` 判定（真实成交价严格越过挂单价即触发 maker 成交，成交量以该笔 print
/// 为预算）。
///
/// `is_buyer_maker = true` 表示买方是挂单方 -> 本笔为主动卖出 (taker 卖)。它是**给策略的
/// 市场微观结构信号**（主动买卖压），**撮合不用它** —— `sim::matcher` 只比价格，见那里的
/// 说明。此前本注释说它是"撮合的成交来源"，与实现矛盾，现予更正。
///
/// 当前没有策略读它，四所各有一套 maker 约定（OKX/HL 由方向反推、Binance 直取 `m`、
/// IBKR 的 trade 路径没有此信息），保留是因为订单流类策略需要它，删了要重新对齐一遍。
#[derive(Debug, Clone)]
pub struct MarketTrade {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub price: Price,
    pub qty: Quantity,
    pub is_buyer_maker: bool,
    pub timestamp: Timestamp,
}

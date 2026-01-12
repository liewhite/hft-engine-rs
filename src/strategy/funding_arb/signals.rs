use crate::domain::Exchange;

/// 交易信号
///
/// 包含做多和做空两边的交易所、价格和数量
/// 统一用于开仓和平仓（反向信号即平仓）
#[derive(Debug, Clone)]
pub struct TradingSignal {
    /// 做多交易所
    pub long_exchange: Exchange,
    /// 做多价格（ask）
    pub long_price: f64,
    /// 做多计划交易量
    pub long_size: f64,
    /// 做多交易所的盘口 ask 数量
    pub long_book_qty: f64,
    /// 做空交易所
    pub short_exchange: Exchange,
    /// 做空价格（bid）
    pub short_price: f64,
    /// 做空计划交易量
    pub short_size: f64,
    /// 做空交易所的盘口 bid 数量
    pub short_book_qty: f64,
    /// 做多方 ask deviation
    pub long_deviation: f64,
    /// 做空方 bid deviation
    pub short_deviation: f64,
}

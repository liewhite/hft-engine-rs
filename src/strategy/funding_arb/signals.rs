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
    /// 做多数量（盘口 ask_qty）
    pub long_size: f64,
    /// 做空交易所
    pub short_exchange: Exchange,
    /// 做空价格（bid）
    pub short_price: f64,
    /// 做空数量（盘口 bid_qty）
    pub short_size: f64,
}

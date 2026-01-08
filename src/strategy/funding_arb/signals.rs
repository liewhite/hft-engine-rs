use crate::domain::Exchange;

/// 开仓信号
#[derive(Debug, Clone)]
pub struct OpenSignal {
    /// bid deviation 最大的交易所（做空/卖出）
    pub short_exchange: Exchange,
    /// 做空价格（bid）
    pub short_price: f64,
    /// ask deviation 最大的交易所（做多/买入）
    pub long_exchange: Exchange,
    /// 做多价格（ask）
    pub long_price: f64,
}

/// 平仓信号
#[derive(Debug, Clone)]
pub struct CloseSignal {
    /// 平多交易所
    pub long_exchange: Exchange,
    /// 平多价格（bid）
    pub long_price: f64,
    /// 平多交易所的持仓量
    pub long_size: f64,
    /// 平空交易所
    pub short_exchange: Exchange,
    /// 平空价格（ask）
    pub short_price: f64,
    /// 平空交易所的持仓量（负数）
    pub short_size: f64,
}

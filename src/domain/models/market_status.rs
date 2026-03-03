use std::fmt;

/// 交易所市场状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarketStatus {
    /// 正常交易时段
    Liquid,
    /// 盘前/盘后
    Extending,
    /// 休市
    Closed,
}

impl fmt::Display for MarketStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarketStatus::Liquid => write!(f, "Liquid"),
            MarketStatus::Extending => write!(f, "Extending"),
            MarketStatus::Closed => write!(f, "Closed"),
        }
    }
}

use serde::{Deserialize, Serialize};
use std::fmt;

/// 交易所枚举：只是身份标签。各所的格式与规则（订单号、价格精度等）归各自的适配层（`crate::exchange::*`），
/// 框架核心与策略不按所分支。
///
/// `Ord` 按声明顺序排序，仅用于让"交易所集合"有确定的展示/遍历顺序（日志与启动可复现），
/// 不含业务优先级语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Exchange {
    Binance,
    OKX,
    Hyperliquid,
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exchange::Binance => write!(f, "Binance"),
            Exchange::OKX => write!(f, "OKX"),
            Exchange::Hyperliquid => write!(f, "Hyperliquid"),
        }
    }
}

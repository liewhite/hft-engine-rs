use crate::domain::models::{Exchange, Side, Symbol};
use crate::domain::types::{Price, Quantity};

/// 仓位信息
///
/// size 为正表示多头，为负表示空头
#[derive(Debug, Clone)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 仓位数量：正数为多头，负数为空头
    pub size: Quantity,
    pub entry_price: Price,
    pub leverage: u32,
    pub unrealized_pnl: f64,
    pub mark_price: Price,
}

impl Position {
    /// 仓位比较的浮点精度阈值
    pub const EPSILON: f64 = 1e-10;

    /// 计算持仓名义价值
    pub fn notional_value(&self) -> f64 {
        self.mark_price * self.size.abs()
    }

    /// 判断是否空仓 (使用 epsilon 比较避免浮点精度问题)
    pub fn is_empty(&self) -> bool {
        self.size.abs() < Self::EPSILON
    }

    /// 获取持仓方向 (根据 size 符号)
    ///
    /// 注意: 空仓 (size == 0) 时返回 Side::Long，调用前应先检查 is_empty()
    pub fn side(&self) -> Side {
        if self.size >= 0.0 {
            Side::Long
        } else {
            Side::Short
        }
    }

    /// 创建空仓位
    pub fn empty(exchange: Exchange, symbol: Symbol) -> Self {
        Self {
            exchange,
            symbol,
            size: 0.0,
            entry_price: 0.0,
            leverage: 1,
            unrealized_pnl: 0.0,
            mark_price: 0.0,
        }
    }
}

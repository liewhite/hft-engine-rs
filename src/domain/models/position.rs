use crate::domain::models::{Exchange, Side, Symbol};
use crate::domain::types::Quantity;

/// 仓位信息
///
/// size 为正表示多头，为负表示空头
#[derive(Debug, Clone)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 仓位数量：正数为多头，负数为空头
    pub size: Quantity,
    pub entry_price: f64,
    pub unrealized_pnl: f64,
}

impl Position {
    /// 仓位比较的浮点精度阈值
    pub const EPSILON: f64 = 1e-10;

    /// 判断是否空仓 (使用 epsilon 比较避免浮点精度问题)
    pub fn is_empty(&self) -> bool {
        self.size.abs() < Self::EPSILON
    }

    /// 获取持仓方向 (根据 size 符号)
    ///
    /// 返回 `None` 表示空仓
    pub fn side(&self) -> Option<Side> {
        if self.is_empty() {
            None
        } else if self.size > 0.0 {
            Some(Side::Long)
        } else {
            Some(Side::Short)
        }
    }

    /// 创建空仓位
    pub fn empty(exchange: Exchange, symbol: Symbol) -> Self {
        Self {
            exchange,
            symbol,
            size: 0.0,
            entry_price: 0.0,
            unrealized_pnl: 0.0,
        }
    }
}

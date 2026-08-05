use crate::domain::models::{Exchange, Side, Symbol};
use crate::domain::types::Quantity;

/// 仓位信息
///
/// size 为正表示多头，为负表示空头
#[derive(Debug, Clone, PartialEq)]
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

    /// 应用一笔成交：维护带符号仓位与**持仓均价**，返回已实现盈亏（不含手续费）。
    ///
    /// 仓位随成交演进的规则只此一处（sim 的 `Ledger` 与策略侧的 `SymbolState` 共用；
    /// 此前策略侧只累加 size，`entry_price` 永远停在首笔值，拿它算止盈/风控的策略
    /// 读到的是烂数据）：
    /// - 新开 / 同向加仓：加权平均成本，已实现盈亏为 0
    /// - 反向平仓：平掉 min(本次, 持仓)，返回已实现盈亏，均价不变（清仓则归零）
    /// - 反手：平掉原仓（返回其已实现盈亏），剩余在成交价重新开仓
    ///
    /// 本方法不触碰 `unrealized_pnl` —— 浮盈需要估值价格，由持有方按自己的行情来源
    /// 维护（`Ledger::open_positions` 按 mark 回填；`SymbolState` 以最新成交价近似）。
    pub fn apply_fill(&mut self, side: Side, price: f64, qty: Quantity) -> f64 {
        let signed = match side {
            Side::Long => qty,
            Side::Short => -qty,
        };
        let old_size = self.size;
        let new_size = old_size + signed;

        if old_size.abs() < Self::EPSILON || (old_size > 0.0) == (signed > 0.0) {
            // 新开 / 同向加仓：加权平均
            self.entry_price = if old_size.abs() < Self::EPSILON {
                price
            } else {
                (old_size.abs() * self.entry_price + qty * price) / (old_size.abs() + qty)
            };
            self.size = new_size;
            0.0
        } else {
            // 反向：平仓 (可能反手)
            let close_qty = qty.min(old_size.abs());
            let dir = if old_size > 0.0 { 1.0 } else { -1.0 };
            let realized = (price - self.entry_price) * close_qty * dir;
            self.entry_price = if signed.abs() <= old_size.abs() {
                if new_size.abs() < Self::EPSILON {
                    0.0
                } else {
                    self.entry_price
                }
            } else {
                price // 反手: 剩余在成交价重开
            };
            self.size = new_size;
            realized
        }
    }
}

use crate::domain::models::{Fill, FillReason, OrderStatus, OrderUpdate, Side};
use std::collections::VecDeque;

/// 保留的最近成交条数（环形缓冲上限，防止长跑内存增长）
pub const RECENT_FILL_CAPACITY: usize = 50;

/// 成交与订单的累计统计
///
/// 纯数据 + 纯函数：只由事件驱动累加，不读时钟、不做 IO，可直接单测。
///
/// 盈亏口径为**现金流 + 存货估值**：
/// - `cash`：卖出收现金、买入付现金，并扣除手续费
/// - 总盈亏 = `cash` + 当前持仓按最新价的市值
///
/// 这样不需要把开仓与平仓配对（跨所、部分成交、强平会让配对逻辑迅速失控），
/// 对任意成交顺序都成立。
#[derive(Debug, Clone, Default)]
pub struct TradingStats {
    /// 成交笔数
    pub fills: u64,
    /// 累计成交名义额 (price * size)
    pub volume: f64,
    /// 累计手续费（正数 = 支出，负数 = 返佣）
    pub fee: f64,
    /// 累计现金流（已扣手续费）：卖出 +notional，买入 -notional
    pub cash: f64,
    /// 强平 / ADL 成交笔数（风险告警指标，正常应恒为 0）
    pub forced_fills: u64,
    /// 被拒绝或出错的订单数
    pub rejected_orders: u64,
    /// 最近成交记录（最新在队尾），容量上限 [`RECENT_FILL_CAPACITY`]
    pub recent_fills: VecDeque<Fill>,
}

impl TradingStats {
    /// 累加一笔成交
    pub fn apply_fill(&mut self, fill: &Fill) {
        let notional = fill.price * fill.size;
        self.fills += 1;
        self.volume += notional;
        self.fee += fill.fee;
        self.cash += match fill.side {
            Side::Long => -notional,
            Side::Short => notional,
        } - fill.fee;

        if fill.reason != FillReason::Normal {
            self.forced_fills += 1;
        }

        if self.recent_fills.len() == RECENT_FILL_CAPACITY {
            self.recent_fills.pop_front();
        }
        self.recent_fills.push_back(fill.clone());
    }

    /// 累加一条订单更新（只统计失败终态）
    pub fn apply_order_update(&mut self, update: &OrderUpdate) {
        match update.status {
            OrderStatus::Rejected { .. } | OrderStatus::Error { .. } => {
                self.rejected_orders += 1;
            }
            _ => {}
        }
    }

    /// 总盈亏 = 累计现金流 + 当前持仓市值
    pub fn total_pnl(&self, position_value: f64) -> f64 {
        self.cash + position_value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Exchange;

    fn fill(side: Side, price: f64, size: f64, fee: f64, reason: FillReason) -> Fill {
        Fill {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            side,
            price,
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: 0,
            fee,
            reason,
        }
    }

    #[test]
    fn round_trip_pnl_equals_spread_minus_fees() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill(Side::Long, 100.0, 1.0, 0.1, FillReason::Normal));
        stats.apply_fill(&fill(Side::Short, 101.0, 1.0, 0.1, FillReason::Normal));

        // 买 100 卖 101，双边各 0.1 手续费 → 净 0.8；平仓后持仓市值为 0
        assert!((stats.total_pnl(0.0) - 0.8).abs() < 1e-9);
        assert_eq!(stats.fills, 2);
        assert!((stats.volume - 201.0).abs() < 1e-9);
        assert!((stats.fee - 0.2).abs() < 1e-9);
    }

    #[test]
    fn open_position_pnl_uses_inventory_value() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill(Side::Long, 100.0, 2.0, 0.0, FillReason::Normal));

        // 持有 2 个、现价 105 → 市值 210，现金流 -200 → 浮盈 10
        assert!((stats.total_pnl(210.0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn counts_forced_fills_separately() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill(Side::Short, 100.0, 1.0, 0.0, FillReason::Liquidation));
        stats.apply_fill(&fill(Side::Short, 100.0, 1.0, 0.0, FillReason::Normal));
        assert_eq!(stats.forced_fills, 1);
        assert_eq!(stats.fills, 2);
    }

    #[test]
    fn recent_fills_are_bounded() {
        let mut stats = TradingStats::default();
        for _ in 0..(RECENT_FILL_CAPACITY + 10) {
            stats.apply_fill(&fill(Side::Long, 1.0, 1.0, 0.0, FillReason::Normal));
        }
        assert_eq!(stats.recent_fills.len(), RECENT_FILL_CAPACITY);
        assert_eq!(stats.fills as usize, RECENT_FILL_CAPACITY + 10);
    }

    #[test]
    fn counts_rejected_orders() {
        let mut stats = TradingStats::default();
        let mut update = OrderUpdate {
            order_id: "1".to_string(),
            client_order_id: None,
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            side: Side::Long,
            status: OrderStatus::Rejected {
                reason: "insufficient margin".to_string(),
            },
            price: 0.0,
            reduce_only: false,
            quantity: 0.0,
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: 0,
        };
        stats.apply_order_update(&update);
        update.status = OrderStatus::Filled;
        stats.apply_order_update(&update);
        assert_eq!(stats.rejected_orders, 1);
    }
}

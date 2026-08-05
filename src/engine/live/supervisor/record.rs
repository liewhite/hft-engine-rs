//! 账户表现记录 —— 晋升判据的**原料**。
//!
//! 判据本身由使用者实现 ([`super::PromotionPolicy`])，本模块只负责把成交流还原成可判定的
//! 事实：累计统计 + **逐次往返的净盈亏**。
//!
//! 为什么要逐次往返、而不是净值曲线：净值里浮亏浮盈占大头，一张还没平的浮亏单会把本来
//! 有效的信号压下去；而"每次往返赚了多少"才是可以做统计的独立样本（笔数、均值、t 值）。

use crate::domain::{Fill, Side, Symbol, Timestamp, TradingStats};

/// 一次完整往返：仓位从 0 开出、再回到 0。
#[derive(Debug, Clone, PartialEq)]
pub struct RoundTrip {
    pub symbol: Symbol,
    pub opened_at: Timestamp,
    pub closed_at: Timestamp,
    /// 本次往返包含的成交笔数
    pub fills: u32,
    /// **净**盈亏：已扣手续费。等于本次往返期间的现金流净额（平仓回到 0 时现金增量即已实现盈亏）
    pub net_pnl: f64,
    /// 开仓侧累计名义额，用于把盈亏归一成收益率（跨 symbol 可比）
    pub open_notional: f64,
}

impl RoundTrip {
    /// 归一化收益：净盈亏 / 开仓名义额。开仓名义额为 0 时返回 0（不构造出无穷大）。
    pub fn return_on_notional(&self) -> f64 {
        if self.open_notional > 0.0 {
            self.net_pnl / self.open_notional
        } else {
            0.0
        }
    }
}

/// 单个账户在单个 symbol 上的表现记录。
///
/// 只消费 [`Fill`]：账户的仓位与现金完全由成交决定，不依赖任何快照，因此模拟账户与实盘账户
/// 可以用同一份实现（二者的 Fill 分别来自本地柜台与交易所私有流）。
#[derive(Debug, Clone)]
pub struct SymbolRecord {
    pub symbol: Symbol,
    /// 累计统计（笔数 / 名义额 / 手续费 / 现金流），复用既有实现
    pub stats: TradingStats,
    /// 已完成的往返，最旧的会被挤出（见 [`Self::round_trip_capacity`]）
    round_trips: Vec<RoundTrip>,
    round_trip_capacity: usize,
    /// 当前仓位（带符号，币本位）
    position: f64,
    /// 累计现金流：卖出 +名义额、买入 −名义额，均已扣手续费
    cash: f64,
    /// 本次往返开始时的现金流水位；`None` = 当前空仓
    open_cash: Option<f64>,
    open_at: Timestamp,
    open_fills: u32,
    open_notional: f64,
}

/// 仓位视为 0 的阈值：浮点累加后极难精确回到 0
const FLAT_EPSILON: f64 = 1e-12;

impl SymbolRecord {
    pub fn new(symbol: Symbol, round_trip_capacity: usize) -> Self {
        Self {
            symbol,
            // 逐笔明细由全局 metrics 保留，这里数百 symbol 各存一份没人读
            stats: TradingStats::with_recent_capacity(0),
            round_trips: Vec::new(),
            round_trip_capacity,
            position: 0.0,
            cash: 0.0,
            open_cash: None,
            open_at: 0,
            open_fills: 0,
            open_notional: 0.0,
        }
    }

    /// 已完成的往返（最旧在前）
    pub fn round_trips(&self) -> &[RoundTrip] {
        &self.round_trips
    }

    pub fn round_trip_capacity(&self) -> usize {
        self.round_trip_capacity
    }

    /// 当前仓位（带符号）
    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn is_flat(&self) -> bool {
        self.position.abs() <= FLAT_EPSILON
    }

    /// 累计已实现盈亏（= 空仓时的现金流净额；持仓时不含浮动盈亏）
    pub fn realized_pnl(&self) -> f64 {
        self.cash
    }

    /// 吃进一笔成交，若因此完成一次往返则返回它。
    ///
    /// **符号翻转的处理**：若一笔成交把仓位从多直接打到空（本项目的平仓单是 reduce-only，
    /// 正常不会发生），把这笔整体计入平仓、随即以同一时刻开出新往返。这是近似，但比丢弃
    /// 或拆分更不容易出错，且记录里能看出来（新往返的 open_fills 从 0 起算）。
    pub fn apply_fill(&mut self, fill: &Fill) -> Option<RoundTrip> {
        self.stats.apply_fill(fill);

        let notional = fill.price * fill.size;
        let signed_qty = match fill.side {
            Side::Long => fill.size,
            Side::Short => -fill.size,
        };
        // 买入付出现金、卖出收到现金，手续费一律为支出
        let cash_delta = match fill.side {
            Side::Long => -notional,
            Side::Short => notional,
        } - fill.fee;

        let was_flat = self.is_flat();
        if was_flat {
            self.open_cash = Some(self.cash);
            self.open_at = fill.timestamp;
            self.open_fills = 0;
            self.open_notional = 0.0;
        }

        self.cash += cash_delta;
        self.position += signed_qty;
        self.open_fills += 1;
        if was_flat || self.position.abs() > (self.position - signed_qty).abs() {
            // 增仓方向：计入开仓名义额
            self.open_notional += notional;
        }

        if !self.is_flat() {
            return None;
        }

        let open_cash = self.open_cash.take().unwrap_or(0.0);
        let trip = RoundTrip {
            symbol: self.symbol.clone(),
            opened_at: self.open_at,
            closed_at: fill.timestamp,
            fills: self.open_fills,
            net_pnl: self.cash - open_cash,
            open_notional: self.open_notional,
        };
        // 浮点残差归零，避免 1e-18 级别的残仓让后续判定成"未平仓"
        self.position = 0.0;
        self.open_fills = 0;
        self.open_notional = 0.0;

        if self.round_trip_capacity > 0 {
            if self.round_trips.len() == self.round_trip_capacity {
                self.round_trips.remove(0);
            }
            self.round_trips.push(trip.clone());
        }
        Some(trip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, FillReason};

    const SYM: &str = "BTC";

    fn fill(side: Side, price: f64, size: f64, fee: f64, ts: Timestamp) -> Fill {
        Fill {
            exchange: Exchange::Binance,
            symbol: SYM.to_string(),
            side,
            price,
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: ts,
            fee,
            reason: FillReason::Normal,
        }
    }

    fn record() -> SymbolRecord {
        SymbolRecord::new(SYM.to_string(), 8)
    }

    #[test]
    fn open_then_close_yields_one_round_trip_net_of_fees() {
        let mut r = record();
        // 买 1 @100，费 0.1
        assert_eq!(r.apply_fill(&fill(Side::Long, 100.0, 1.0, 0.1, 10)), None);
        assert!(!r.is_flat());
        // 卖 1 @101，费 0.1 -> 毛赚 1，净 0.8
        let trip = r
            .apply_fill(&fill(Side::Short, 101.0, 1.0, 0.1, 20))
            .expect("往返完成");
        assert!((trip.net_pnl - 0.8).abs() < 1e-12, "got {}", trip.net_pnl);
        assert_eq!(trip.fills, 2);
        assert_eq!(trip.opened_at, 10);
        assert_eq!(trip.closed_at, 20);
        assert!((trip.open_notional - 100.0).abs() < 1e-12);
        assert!(r.is_flat());
        assert!((r.realized_pnl() - 0.8).abs() < 1e-12);
    }

    /// 亏损往返同样被记录（判据需要看到负样本，否则统计毫无意义）
    #[test]
    fn losing_round_trip_is_recorded() {
        let mut r = record();
        r.apply_fill(&fill(Side::Long, 100.0, 1.0, 0.0, 1));
        let trip = r.apply_fill(&fill(Side::Short, 99.0, 1.0, 0.0, 2)).unwrap();
        assert!((trip.net_pnl + 1.0).abs() < 1e-12);
        assert!((trip.return_on_notional() + 0.01).abs() < 1e-12);
    }

    /// 分批建仓/分批平仓：只在回到空仓时算一次往返
    #[test]
    fn partial_scaling_counts_as_single_round_trip() {
        let mut r = record();
        assert!(r.apply_fill(&fill(Side::Long, 100.0, 1.0, 0.0, 1)).is_none());
        assert!(r.apply_fill(&fill(Side::Long, 102.0, 1.0, 0.0, 2)).is_none());
        assert!(r.apply_fill(&fill(Side::Short, 103.0, 1.0, 0.0, 3)).is_none());
        let trip = r.apply_fill(&fill(Side::Short, 103.0, 1.0, 0.0, 4)).unwrap();
        // 成本 202，收入 206 -> 净 4
        assert!((trip.net_pnl - 4.0).abs() < 1e-12, "got {}", trip.net_pnl);
        assert_eq!(trip.fills, 4);
        // 开仓名义额只累计增仓侧
        assert!((trip.open_notional - 202.0).abs() < 1e-12);
    }

    #[test]
    fn short_side_round_trip_is_symmetric() {
        let mut r = record();
        r.apply_fill(&fill(Side::Short, 100.0, 1.0, 0.0, 1));
        let trip = r.apply_fill(&fill(Side::Long, 99.0, 1.0, 0.0, 2)).unwrap();
        assert!((trip.net_pnl - 1.0).abs() < 1e-12, "做空下跌应盈利");
    }

    /// 容量上限：最旧的往返被挤出，累计已实现盈亏不受影响
    #[test]
    fn round_trips_are_capped_but_realized_pnl_is_not() {
        let mut r = SymbolRecord::new(SYM.to_string(), 2);
        for i in 0..4u64 {
            r.apply_fill(&fill(Side::Long, 100.0, 1.0, 0.0, i * 10));
            r.apply_fill(&fill(Side::Short, 101.0, 1.0, 0.0, i * 10 + 1));
        }
        assert_eq!(r.round_trips().len(), 2);
        assert_eq!(r.round_trips()[0].opened_at, 20, "只保留最近两次");
        assert!((r.realized_pnl() - 4.0).abs() < 1e-12, "累计盈亏含全部 4 次");
    }

    /// 浮点残差不应让账户看起来还有仓位
    #[test]
    fn tiny_float_residual_is_treated_as_flat() {
        let mut r = record();
        r.apply_fill(&fill(Side::Long, 100.0, 0.1, 0.0, 1));
        r.apply_fill(&fill(Side::Long, 100.0, 0.2, 0.0, 2));
        let trip = r.apply_fill(&fill(Side::Short, 100.0, 0.3, 0.0, 3));
        assert!(trip.is_some(), "0.1+0.2-0.3 的残差必须视为空仓");
        assert!(r.is_flat());
        assert_eq!(r.position(), 0.0);
    }
}

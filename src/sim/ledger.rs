use crate::domain::{signed_qty, Price, Quantity, Side, Symbol};
use std::collections::HashMap;

/// 账本里的一笔持仓：数量 + **本地维护的持仓均价**。
///
/// 与域模型 [`crate::domain::Position`] 的分工是本设计的要点：那边只镜像"交易所上现在持有
/// 多少"（策略决策只需要数量），**均价是记账概念，只属于账本**。均价在这里完全由本地成交流
/// 加权得出，不读、也不需要对齐任何交易所的 `avgPx`/`entryPx`/`avgCost` —— 这就是为什么
/// 交易所适配层可以对这些字段一无所知。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookPosition {
    /// 带符号数量：正多负空
    pub size: Quantity,
    /// 持仓均价（本地按成交加权，见 [`Self::apply_fill`]）
    pub entry_price: Price,
}

impl BookPosition {
    /// 空仓判定的浮点阈值（与域模型同一口径）
    const EPSILON: f64 = crate::domain::Position::EPSILON;

    pub fn is_empty(&self) -> bool {
        self.size.abs() < Self::EPSILON
    }

    /// 应用一笔成交：维护带符号数量与持仓均价，返回**已实现盈亏**（不含手续费）。
    ///
    /// 记账规则的唯一出处：
    /// - 新开 / 同向加仓：加权平均成本，已实现盈亏为 0
    /// - 反向平仓：平掉 min(本次, 持仓)，返回其已实现盈亏，均价不变（清仓则归零）
    /// - 反手：平掉原仓（返回其已实现盈亏），剩余在成交价重新开仓
    pub fn apply_fill(&mut self, side: Side, price: Price, qty: Quantity) -> f64 {
        let signed = signed_qty(side, qty);
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
            // 反向：平仓（可能反手）
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
                price // 反手：剩余在成交价重开
            };
            self.size = new_size;
            realized
        }
    }
}

/// 账本快照里的一笔持仓（含按估值价算出的未实现盈亏），用于回测结果与柜台净值展示。
#[derive(Debug, Clone, PartialEq)]
pub struct BookSnapshot {
    pub symbol: Symbol,
    pub size: Quantity,
    pub entry_price: Price,
    pub unrealized_pnl: f64,
}

/// 纯账本：仓位 + 现金，无副作用、无锁，可脱离线程同步单测 (与 ox-demo `Ledger` 同构)。
///
/// 一份账本只代表**一个交易所上的一个账户**（交易所标识由持有者 `SimState` 持有，账本自身
/// 不需要知道），故 `positions` 以 symbol 为键。
#[derive(Debug, Clone)]
pub struct Ledger {
    pub positions: HashMap<Symbol, BookPosition>,
    pub cash: f64,
}

impl Ledger {
    pub fn empty(cash: f64) -> Self {
        Self {
            positions: HashMap::new(),
            cash,
        }
    }

    /// 应用一笔成交，就地更新账本：仓位/均价由 [`BookPosition::apply_fill`] 演进，
    /// 账本只负责把已实现盈亏落进现金。
    ///
    /// `fee` (>=0) 直接从现金扣除；maker/taker 区分与费率换算由调用方 (SimState) 决定。
    pub fn apply_fill(
        &mut self,
        symbol: &Symbol,
        side: Side,
        price: Price,
        qty: Quantity,
        fee: f64,
    ) {
        let pos = self.positions.entry(symbol.clone()).or_default();
        let realized = pos.apply_fill(side, price, qty);
        self.cash += realized - fee;
    }

    /// 账户净值 = 现金 + 未实现盈亏 (`mark_of` 提供各 symbol 的估值价格)
    pub fn equity(&self, mark_of: impl Fn(&Symbol) -> f64) -> f64 {
        self.cash
            + self
                .positions
                .iter()
                .map(|(symbol, p)| unrealized_pnl(p, mark_of(symbol)))
                .sum::<f64>()
    }

    /// 总持仓名义价值 (用于杠杆率)
    pub fn notional(&self, mark_of: impl Fn(&Symbol) -> f64) -> f64 {
        self.positions
            .iter()
            .map(|(symbol, p)| p.size.abs() * mark_of(symbol))
            .sum()
    }

    /// 非空仓位的快照（回填按 `mark_of` 算出的未实现盈亏）
    pub fn open_positions(&self, mark_of: impl Fn(&Symbol) -> f64) -> Vec<BookSnapshot> {
        self.positions
            .iter()
            .filter(|(_, p)| !p.is_empty())
            .map(|(symbol, p)| BookSnapshot {
                symbol: symbol.clone(),
                size: p.size,
                entry_price: p.entry_price,
                unrealized_pnl: unrealized_pnl(p, mark_of(symbol)),
            })
            .collect()
    }
}

/// 未实现盈亏：(标记价 - 均价) * 带符号仓位；无估值价格 (mark<=0) 时记 0
fn unrealized_pnl(pos: &BookPosition, mark: f64) -> f64 {
    if mark <= 0.0 {
        0.0
    } else {
        (mark - pos.entry_price) * pos.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym() -> Symbol {
        "BTCUSDT".to_string()
    }
    fn empty() -> Ledger {
        Ledger::empty(10_000.0)
    }
    fn size_of(l: &Ledger) -> f64 {
        l.positions.get(&sym()).map(|p| p.size).unwrap_or(0.0)
    }
    fn entry_of(l: &Ledger) -> f64 {
        l.positions.get(&sym()).map(|p| p.entry_price).unwrap_or(0.0)
    }

    #[test]
    fn open_long_records_entry_cash_unchanged() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 2.0, 0.0);
        assert_eq!(size_of(&l), 2.0);
        assert_eq!(entry_of(&l), 100.0);
        assert_eq!(l.cash, 10_000.0);
    }

    #[test]
    fn add_same_side_weighted_average() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 2.0, 0.0);
        l.apply_fill(&sym(), Side::Long, 110.0, 2.0, 0.0);
        assert_eq!(size_of(&l), 4.0);
        assert_eq!(entry_of(&l), 105.0); // (2*100 + 2*110)/4
        assert_eq!(l.cash, 10_000.0);
    }

    #[test]
    fn partial_close_realizes_pnl_entry_unchanged() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 4.0, 0.0);
        l.apply_fill(&sym(), Side::Short, 120.0, 1.0, 0.0); // 平 1, 盈利 20
        assert_eq!(size_of(&l), 3.0);
        assert_eq!(entry_of(&l), 100.0);
        assert_eq!(l.cash, 10_020.0);
    }

    #[test]
    fn full_close_zeroes_position_and_entry() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 2.0, 0.0);
        l.apply_fill(&sym(), Side::Short, 90.0, 2.0, 0.0); // 平 2, 亏损 20
        assert_eq!(size_of(&l), 0.0);
        assert_eq!(entry_of(&l), 0.0);
        assert_eq!(l.cash, 9_980.0);
    }

    #[test]
    fn reverse_closes_then_reopens_remainder() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 2.0, 0.0);
        l.apply_fill(&sym(), Side::Short, 120.0, 5.0, 0.0); // 平多 2(+40), 反手开空 3 @120
        assert_eq!(size_of(&l), -3.0);
        assert_eq!(entry_of(&l), 120.0);
        assert_eq!(l.cash, 10_040.0);
    }

    #[test]
    fn short_close_pnl_direction() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Short, 100.0, 2.0, 0.0);
        l.apply_fill(&sym(), Side::Long, 90.0, 2.0, 0.0); // 空头平于低价, 盈利 20
        assert_eq!(size_of(&l), 0.0);
        assert_eq!(l.cash, 10_020.0);
    }

    #[test]
    fn equity_and_notional_use_unrealized() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 2.0, 0.0);
        let mark_of = |_: &Symbol| 150.0;
        assert_eq!(l.equity(mark_of), 10_000.0 + (150.0 - 100.0) * 2.0); // 10100
        assert_eq!(l.notional(mark_of), 2.0 * 150.0); // 300
        assert_eq!(l.open_positions(mark_of)[0].unrealized_pnl, 100.0);
    }

    #[test]
    fn no_mark_price_zero_unrealized() {
        let mut l = empty();
        l.apply_fill(&sym(), Side::Long, 100.0, 2.0, 0.0);
        assert_eq!(l.equity(|_: &Symbol| 0.0), 10_000.0);
    }
}

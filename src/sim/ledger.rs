use crate::domain::{Exchange, Position, Price, Quantity, Side, Symbol};
use std::collections::HashMap;

/// 纯账本：仓位 + 现金，无副作用、无锁，可脱离线程同步单测 (与 ox-demo `Ledger` 同构)。
///
/// 仓位 `size` 带符号 (正多负空)，`entry_price` 为持仓均价，现金累加已实现盈亏。
#[derive(Debug, Clone)]
pub struct Ledger {
    pub positions: HashMap<Symbol, Position>,
    pub cash: f64,
}

impl Ledger {
    pub fn empty(cash: f64) -> Self {
        Self {
            positions: HashMap::new(),
            cash,
        }
    }

    /// 应用一笔成交，就地更新账本：
    ///   - 新开 / 同向加仓：加权平均成本
    ///   - 反向平仓：平掉 min(本次, 持仓) 的已实现盈亏入现金
    ///   - 反手：平掉原仓后，剩余在成交价重新开仓
    ///
    /// `fee` (>=0) 直接从现金扣除；maker/taker 区分与费率换算由调用方 (SimState) 决定。
    pub fn apply_fill(
        &mut self,
        exchange: Exchange,
        symbol: &Symbol,
        side: Side,
        price: Price,
        qty: Quantity,
        fee: f64,
    ) {
        let signed = match side {
            Side::Long => qty,
            Side::Short => -qty,
        };
        let pos = self
            .positions
            .entry(symbol.clone())
            .or_insert_with(|| Position::empty(exchange, symbol.clone()));
        let old_size = pos.size;
        let new_size = old_size + signed;

        if old_size.abs() < Position::EPSILON || (old_size > 0.0) == (signed > 0.0) {
            // 新开 / 同向加仓
            let new_entry = if old_size.abs() < Position::EPSILON {
                price
            } else {
                (old_size.abs() * pos.entry_price + qty * price) / (old_size.abs() + qty)
            };
            pos.size = new_size;
            pos.entry_price = new_entry;
            self.cash -= fee;
        } else {
            // 反向：平仓 (可能反手)
            let close_qty = qty.min(old_size.abs());
            let dir = if old_size > 0.0 { 1.0 } else { -1.0 };
            let realized = (price - pos.entry_price) * close_qty * dir;
            let new_entry = if signed.abs() <= old_size.abs() {
                if new_size.abs() < Position::EPSILON {
                    0.0
                } else {
                    pos.entry_price
                }
            } else {
                price // 反手: 剩余在成交价重开
            };
            pos.size = new_size;
            pos.entry_price = new_entry;
            self.cash += realized - fee;
        }
    }

    /// 账户净值 = 现金 + 未实现盈亏 (`mark_of` 提供各 symbol 的估值价格)
    pub fn equity(&self, mark_of: impl Fn(&Symbol) -> f64) -> f64 {
        self.cash
            + self
                .positions
                .values()
                .map(|p| unrealized_pnl(p, mark_of(&p.symbol)))
                .sum::<f64>()
    }

    /// 总持仓名义价值 (用于杠杆率)
    pub fn notional(&self, mark_of: impl Fn(&Symbol) -> f64) -> f64 {
        self.positions
            .values()
            .map(|p| p.size.abs() * mark_of(&p.symbol))
            .sum()
    }

    /// 非空仓位 (回填最新未实现盈亏)
    pub fn open_positions(&self, mark_of: impl Fn(&Symbol) -> f64) -> Vec<Position> {
        self.positions
            .values()
            .filter(|p| !p.is_empty())
            .map(|p| {
                let mut q = p.clone();
                q.unrealized_pnl = unrealized_pnl(p, mark_of(&p.symbol));
                q
            })
            .collect()
    }
}

/// 未实现盈亏：(标记价 - 均价) * 带符号仓位；无估值价格 (mark<=0) 时记 0
fn unrealized_pnl(pos: &Position, mark: f64) -> f64 {
    if mark <= 0.0 {
        0.0
    } else {
        (mark - pos.entry_price) * pos.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EX: Exchange = Exchange::Binance;

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
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 2.0, 0.0);
        assert_eq!(size_of(&l), 2.0);
        assert_eq!(entry_of(&l), 100.0);
        assert_eq!(l.cash, 10_000.0);
    }

    #[test]
    fn add_same_side_weighted_average() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 2.0, 0.0);
        l.apply_fill(EX, &sym(), Side::Long, 110.0, 2.0, 0.0);
        assert_eq!(size_of(&l), 4.0);
        assert_eq!(entry_of(&l), 105.0); // (2*100 + 2*110)/4
        assert_eq!(l.cash, 10_000.0);
    }

    #[test]
    fn partial_close_realizes_pnl_entry_unchanged() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 4.0, 0.0);
        l.apply_fill(EX, &sym(), Side::Short, 120.0, 1.0, 0.0); // 平 1, 盈利 20
        assert_eq!(size_of(&l), 3.0);
        assert_eq!(entry_of(&l), 100.0);
        assert_eq!(l.cash, 10_020.0);
    }

    #[test]
    fn full_close_zeroes_position_and_entry() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 2.0, 0.0);
        l.apply_fill(EX, &sym(), Side::Short, 90.0, 2.0, 0.0); // 平 2, 亏损 20
        assert_eq!(size_of(&l), 0.0);
        assert_eq!(entry_of(&l), 0.0);
        assert_eq!(l.cash, 9_980.0);
    }

    #[test]
    fn reverse_closes_then_reopens_remainder() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 2.0, 0.0);
        l.apply_fill(EX, &sym(), Side::Short, 120.0, 5.0, 0.0); // 平多 2(+40), 反手开空 3 @120
        assert_eq!(size_of(&l), -3.0);
        assert_eq!(entry_of(&l), 120.0);
        assert_eq!(l.cash, 10_040.0);
    }

    #[test]
    fn short_close_pnl_direction() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Short, 100.0, 2.0, 0.0);
        l.apply_fill(EX, &sym(), Side::Long, 90.0, 2.0, 0.0); // 空头平于低价, 盈利 20
        assert_eq!(size_of(&l), 0.0);
        assert_eq!(l.cash, 10_020.0);
    }

    #[test]
    fn equity_and_notional_use_unrealized() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 2.0, 0.0);
        let mark_of = |_: &Symbol| 150.0;
        assert_eq!(l.equity(mark_of), 10_000.0 + (150.0 - 100.0) * 2.0); // 10100
        assert_eq!(l.notional(mark_of), 2.0 * 150.0); // 300
        assert_eq!(l.open_positions(mark_of)[0].unrealized_pnl, 100.0);
    }

    #[test]
    fn no_mark_price_zero_unrealized() {
        let mut l = empty();
        l.apply_fill(EX, &sym(), Side::Long, 100.0, 2.0, 0.0);
        assert_eq!(l.equity(|_: &Symbol| 0.0), 10_000.0);
    }
}

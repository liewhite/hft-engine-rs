use crate::domain::models::{Exchange, Side, Symbol};
use crate::domain::types::Quantity;

/// 持仓：**只有数量**。
///
/// # 为什么不带均价与盈亏
///
/// 本类型是"某个交易所上某个 symbol 现在持有多少"的镜像，服务的是**交易决策**——而策略
/// 决策只需要数量：还能加多少仓、要平多少、两条腿净敞口是否为零。均价与盈亏曾经也在这里，
/// 结果是纯负担：
///
/// - **四个交易所各报各的**：`avgPx` / `entryPx` / `avgCost` / `entryPrice`，空仓时的表达
///   也各不相同（空串 / `null` / 缺键），为了对齐它们写了一整套"持仓非零时价格字段必须有值"
///   的守卫，还得区分"合法缺失"与"解析失败"；
/// - **而没有任何策略读它**：唯一的下游是把它算成未实现盈亏，那个字段又没有读者。
///
/// 需要盈亏的地方都不该从这里取：
/// - 模拟柜台与回测的净值/已实现盈亏由本地账本自己维护（[`crate::sim::BookPosition`]，
///   均价按成交流加权，与交易所报什么无关）；
/// - 实盘账户净值走 [`crate::messaging::ExchangeEventData::AccountInfo`]（交易所算好的，
///   一个数、无需逐 symbol 对齐）；
/// - 策略需要成本时自己从成交流记，口径由它自己定。
///
/// `size` 为正表示多头，为负表示空头。
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 仓位数量：正数为多头，负数为空头
    pub size: Quantity,
}

impl Position {
    /// 是否空仓（数量是精确十进制，直接判 0）
    pub fn is_empty(&self) -> bool {
        self.size.is_zero()
    }

    /// 获取持仓方向 (根据 size 符号)
    ///
    /// 返回 `None` 表示空仓
    pub fn side(&self) -> Option<Side> {
        if self.is_empty() {
            None
        } else if self.size.is_sign_positive() {
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
            size: Quantity::ZERO,
        }
    }

    /// 按一笔成交累加带符号数量。
    ///
    /// 这就是持仓随成交演进的**全部内容**（见结构文档：均价与盈亏不在域模型里）。
    /// 买加、卖减，与"开仓/平仓/反手"无关——那些区分只有记账才需要，而记账在
    /// [`crate::sim::BookPosition`] 里完成。
    pub fn add_fill(&mut self, side: Side, qty: Quantity) {
        self.size += signed_qty(side, qty);
    }
}

/// 成交数量的带符号表达：买为正、卖为负（持仓与账本共用同一口径）
pub fn signed_qty(side: Side, qty: Quantity) -> Quantity {
    match side {
        Side::Long => qty,
        Side::Short => -qty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn pos(size: Quantity) -> Position {
        Position {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            size,
        }
    }

    /// 成交累加只动数量：买加卖减，反手也只是符号翻过去
    #[test]
    fn add_fill_accumulates_signed_size() {
        let mut p = pos(dec!(0));
        p.add_fill(Side::Long, dec!(2));
        assert_eq!(p.size, dec!(2));
        p.add_fill(Side::Long, dec!(1));
        assert_eq!(p.size, dec!(3));
        p.add_fill(Side::Short, dec!(5));
        assert_eq!(p.size, dec!(-2), "反手后应为净空头");
    }

    /// 开平同量后精确归零（f64 时 0.1+0.2-0.3 留残差，曾要 epsilon 判空仓）
    #[test]
    fn flat_after_offsetting_fills() {
        let mut p = pos(dec!(0));
        p.add_fill(Side::Long, dec!(0.1));
        p.add_fill(Side::Long, dec!(0.2));
        p.add_fill(Side::Short, dec!(0.3));
        assert!(p.is_empty());
        assert_eq!(p.side(), None);
        assert_eq!(pos(dec!(1)).side(), Some(Side::Long));
        assert_eq!(pos(dec!(-1)).side(), Some(Side::Short));
    }
}

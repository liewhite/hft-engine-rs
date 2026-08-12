//! `SubscriptionScope` —— 一个消费者的订阅范围，以及"这条事件归不归它"的**唯一**判据。
//!
//! # 为什么要有这个类型
//!
//! 同一个判断有两条驱动要做：实盘的 `IncomeProcessorActor`（总线 → executor）与回测的
//! `BacktestEngine`（虚拟时间循环 → runner）。此前两边各写各的 —— 一个查
//! `sub.symbols.contains(&key)`、一个查 `self.subscriptions.contains(&route)` ——
//! 恰好等价，但那是巧合不是保证（原则 P1/P4）。
//!
//! 判据收进本类型之后，两条驱动调的是同一个 [`SubscriptionScope::accepts`]，
//! 而"事件的定位精度有几档"由 [`Delivery`] 的穷举 `match` 保证漏不掉（原则 P6）。
//!
//! # 热路径上仍然可以用索引
//!
//! `IncomeProcessorActor` 为 symbol 定向的行情事件建了 `(所, symbol) -> 订阅者` 的反向
//! 索引，那是 O(1) 扇出优化，不是第二份判据 —— 索引的键由 [`SubscriptionScope::pairs`]
//! 灌，与 `accepts` 同源。

use crate::domain::{Exchange, Symbol};
use crate::messaging::{Delivery, IncomeEvent};
use std::collections::{HashMap, HashSet};

/// 一个消费者订阅的 (交易所, symbol) 范围。
#[derive(Debug, Clone, Default)]
pub struct SubscriptionScope {
    /// 所 -> 该所订阅的 symbol 集合。
    ///
    /// 按所分层而非存 `HashSet<(Exchange, Symbol)>`，有两个理由：**键集兼作订阅交易所
    /// 集合**（不另存副本，见 P1），且判定 symbol 时不必现场拼一个带 `String` 的元组当
    /// 键 —— 那在每条 BBO 上都是一次分配。
    symbols: HashMap<Exchange, HashSet<Symbol>>,
}

impl SubscriptionScope {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (Exchange, Symbol)>) -> Self {
        let mut symbols: HashMap<Exchange, HashSet<Symbol>> = HashMap::new();
        for (exchange, symbol) in pairs {
            symbols.entry(exchange).or_default().insert(symbol);
        }
        Self { symbols }
    }

    /// 这条事件在本范围内吗 —— **判据的唯一出处**。
    ///
    /// 所级读数按所过滤，是三档里唯一不显然的一档：`Balance` / `AccountInfo` / `Greeks`
    /// 没有 symbol，此前一律广播，于是策略能读到自己压根没订的所的净值 —— 而杠杆闸门
    /// 就是拿净值算的。堵在这里（分发层）而不是堵在策略视图上：视图只挡查询，挡不住
    /// 事件 payload 本身带着净值推给策略。
    pub fn accepts(&self, event: &IncomeEvent) -> bool {
        match event.delivery() {
            Delivery::Symbol(exchange, symbol) => self
                .symbols
                .get(&exchange)
                .is_some_and(|symbols| symbols.contains(&symbol)),
            Delivery::Exchange(exchange) => self.symbols.contains_key(&exchange),
            Delivery::Broadcast => true,
        }
    }

    /// 遍历全部 (所, symbol) 对（供分发层灌反向索引）
    pub fn pairs(&self) -> impl Iterator<Item = (Exchange, &Symbol)> {
        self.symbols
            .iter()
            .flat_map(|(exchange, symbols)| symbols.iter().map(move |s| (*exchange, s)))
    }

    /// 遍历全部 symbol（可能跨所重复）
    pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.values().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountId, BBO};
    use crate::messaging::{AccountData, MarketData};

    const EX: Exchange = Exchange::OKX;
    const OUTSIDE: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn scope() -> SubscriptionScope {
        SubscriptionScope::from_pairs([(EX, SYM.to_string())])
    }

    fn bbo(exchange: Exchange, symbol: &str) -> IncomeEvent {
        IncomeEvent::market(
            0,
            0,
            MarketData::BBO(BBO {
                exchange,
                symbol: symbol.to_string(),
                bid_price: 1.0,
                bid_qty: 1.0,
                ask_price: 1.0,
                ask_qty: 1.0,
                timestamp: 0,
            }),
        )
    }

    fn account_info(exchange: Exchange) -> IncomeEvent {
        IncomeEvent::account(
            AccountId::Live,
            0,
            0,
            AccountData::AccountInfo {
                exchange,
                equity: 1.0,
                notional: 0.0,
            },
        )
    }

    /// 定向档：只收订了的 (所, symbol)
    #[test]
    fn symbol_events_need_both_the_exchange_and_the_symbol() {
        let scope = scope();
        assert!(scope.accepts(&bbo(EX, SYM)));
        assert!(!scope.accepts(&bbo(EX, "ETH")), "未订阅的 symbol 不该收");
        assert!(!scope.accepts(&bbo(OUTSIDE, SYM)), "未订阅的所不该收");
    }

    /// **越界防线**：所级账户读数按所过滤。
    ///
    /// 写成广播的那版里，策略能读到自己压根没订的所的净值与希腊值，
    /// 而 `spread_arb` 的杠杆闸门、`gamma_scalp` 的 delta 都是拿这些算的。
    #[test]
    fn exchange_level_reads_are_confined_to_subscribed_exchanges() {
        let scope = scope();
        assert!(scope.accepts(&account_info(EX)));
        assert!(
            !scope.accepts(&account_info(OUTSIDE)),
            "未订阅所的净值不该到达消费者"
        );
    }

    /// 广播档：与交易所无关的全局事件一律放行（Clock 不放行会让超时清理停摆）
    #[test]
    fn events_without_an_exchange_reach_everyone() {
        assert!(scope().accepts(&IncomeEvent::market(0, 0, MarketData::Clock)));
    }

    /// 空范围只收广播 —— 不会因为"没订任何东西"就变成什么都收
    #[test]
    fn an_empty_scope_still_only_takes_broadcasts() {
        let empty = SubscriptionScope::default();
        assert!(empty.accepts(&IncomeEvent::market(0, 0, MarketData::Clock)));
        assert!(!empty.accepts(&bbo(EX, SYM)));
        assert!(!empty.accepts(&account_info(EX)));
    }
}

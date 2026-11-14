//! `SubscriptionScope` —— 一个消费者的订阅范围，以及"这条事件归不归它"的**唯一**判据。
//!
//! # 为什么要有这个类型
//!
//! 同一个判断有两条驱动要做：实盘的 `IncomeProcessor`（总线 → executor）与回测的
//! `BacktestEngine`（虚拟时间循环 → runner）。判据收进本类型之后，两条驱动调的是同一个
//! [`SubscriptionScope::accepts`]：账户归属、所、symbol 三个维度都在这里判，不在分发层
//! 另写一份账户过滤（原则 P1/P4）。"事件的定位精度有几档"由 [`Delivery`] 的穷举 `match`
//! 保证漏不掉（原则 P6）。
//!
//! # 热路径上仍然可以用索引
//!
//! `IncomeProcessor` 为 symbol 定向的行情事件建了 `(所, symbol) -> 订阅者` 的反向
//! 索引、为账户事件建了 `账户 -> 订阅者` 的索引，那是扇出优化，不是第二份判据 ——
//! 索引的键由 [`SubscriptionScope::pairs`] / [`SubscriptionScope::account`] 灌，与判据同源。
//! 这条同源关系是**结构约定**而非编译期保证，故分发层在索引命中处留了
//! `debug_assert`：索引与范围一旦失配，测试与回测里会立刻响，而不是静默错投。

use crate::domain::{AccountId, Exchange, Symbol};
use crate::messaging::{Delivery, IncomeEvent};
use std::collections::{HashMap, HashSet};

/// 一个消费者的订阅范围：绑定的账户 + 订阅的 (交易所, symbol)。
#[derive(Debug, Clone)]
pub struct SubscriptionScope {
    /// 绑定的账户：只收这个账户的私有事件（行情无账户归属，不受它限制）
    account: AccountId,
    /// 所 -> 该所订阅的 symbol 集合。
    ///
    /// 按所分层而非存 `HashSet<(Exchange, Symbol)>`，有两个理由：**键集兼作订阅交易所
    /// 集合**（不另存副本，见 P1），且判定 symbol 时不必现场拼一个带 `String` 的元组当
    /// 键 —— 那在每条 BBO 上都是一次分配。
    symbols: HashMap<Exchange, HashSet<Symbol>>,
}

impl SubscriptionScope {
    pub fn new(account: AccountId, pairs: impl IntoIterator<Item = (Exchange, Symbol)>) -> Self {
        let mut symbols: HashMap<Exchange, HashSet<Symbol>> = HashMap::new();
        for (exchange, symbol) in pairs {
            symbols.entry(exchange).or_default().insert(symbol);
        }
        Self { account, symbols }
    }

    /// 这条事件在本范围内吗 —— 判据的**唯一出处**。
    ///
    /// - 账户事件先看归属：别的账户的私有事件一律不收（实盘与模拟账户不串账）。
    /// - 所级读数按所过滤：`Balance` / `Equity` / `Greeks` 没有 symbol，写成广播的话
    ///   策略能读到自己压根没订的所的净值。
    pub fn accepts(&self, event: &IncomeEvent) -> bool {
        match event {
            IncomeEvent::Market(m) => self.covers(&m.data.delivery()),
            IncomeEvent::Account(a) => a.account == self.account && self.covers(&a.data.delivery()),
        }
    }

    /// 本范围订阅了这个 (所, symbol) 吗（下单范围闸门也用它，见 `StrategyRunner`）
    pub fn contains(&self, exchange: Exchange, symbol: &Symbol) -> bool {
        self.symbols
            .get(&exchange)
            .is_some_and(|symbols| symbols.contains(symbol))
    }

    fn covers(&self, delivery: &Delivery) -> bool {
        match delivery {
            Delivery::Symbol(exchange, symbol) => self.contains(*exchange, symbol),
            Delivery::Exchange(exchange) => self.symbols.contains_key(exchange),
            Delivery::Broadcast => true,
        }
    }

    /// 绑定的账户（供分发层灌账户索引）
    pub fn account(&self) -> &AccountId {
        &self.account
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
    use crate::domain::BBO;
    use crate::messaging::SymbolMarketData;
    use crate::messaging::{AccountData, ExchangeAccountData, MarketData};
    use rust_decimal_macros::dec;

    const EX: Exchange = Exchange::OKX;
    const OUTSIDE: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn scope() -> SubscriptionScope {
        SubscriptionScope::new(AccountId::Live, [(EX, SYM.to_string())])
    }

    fn bbo(exchange: Exchange, symbol: &str) -> IncomeEvent {
        IncomeEvent::market(
            0,
            0,
            MarketData::Symbol(SymbolMarketData::BBO(BBO {
                exchange,
                symbol: symbol.to_string(),
                bid_price: dec!(1.0),
                bid_qty: dec!(1.0),
                ask_price: dec!(1.0),
                ask_qty: dec!(1.0),
                timestamp: 0,
            })),
        )
    }

    fn account_info_of(account: AccountId, exchange: Exchange) -> IncomeEvent {
        IncomeEvent::account(
            account,
            0,
            0,
            AccountData::Exchange(ExchangeAccountData::Equity {
                exchange,
                equity: crate::domain::Money::new(1.0, "USDT"),
            }),
        )
    }

    fn account_info(exchange: Exchange) -> IncomeEvent {
        account_info_of(AccountId::Live, exchange)
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
    /// 而策略的判断是拿这些算的。
    #[test]
    fn exchange_level_reads_are_confined_to_subscribed_exchanges() {
        let scope = scope();
        assert!(scope.accepts(&account_info(EX)));
        assert!(
            !scope.accepts(&account_info(OUTSIDE)),
            "未订阅所的净值不该到达消费者"
        );
    }

    /// **串账防线**：别的账户的私有事件，就算所和 symbol 都对得上也不收；行情不分账户
    #[test]
    fn account_events_of_another_account_are_rejected() {
        let paper =
            SubscriptionScope::new(AccountId::Paper("x".to_string()), [(EX, SYM.to_string())]);
        assert!(!paper.accepts(&account_info(EX)), "模拟账户不该收实盘净值");
        assert!(paper.accepts(&account_info_of(AccountId::Paper("x".to_string()), EX)));
        assert!(paper.accepts(&bbo(EX, SYM)), "行情无账户归属");
    }

    /// 广播档：与交易所无关的全局事件一律放行（Clock 不放行会让超时清理停摆）
    #[test]
    fn events_without_an_exchange_reach_everyone() {
        assert!(scope().accepts(&IncomeEvent::market(0, 0, MarketData::Clock)));
    }

    /// 空范围只收广播 —— 不会因为"没订任何东西"就变成什么都收
    #[test]
    fn an_empty_scope_still_only_takes_broadcasts() {
        let empty = SubscriptionScope::new(AccountId::Live, []);
        assert!(empty.accepts(&IncomeEvent::market(0, 0, MarketData::Clock)));
        assert!(!empty.accepts(&bbo(EX, SYM)));
        assert!(!empty.accepts(&account_info(EX)));
    }
}

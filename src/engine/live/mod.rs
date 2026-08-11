//! Engine Actor 模块
//!
//! 包含引擎中各个 Actor 的实现

pub mod assembly;
mod clock;
mod crypto_status;
mod manager;
mod executor;
mod income_processor;
mod metrics;
mod outcome_processor;
mod paper_counter;
mod position_ledger;
mod supervisor;

use crate::domain::AccountId;
use crate::messaging::{AccountEvent, MarketEvent};
use crate::strategy::OutcomeEvent;
use kameo_actors::pubsub::PubSub;

/// 公共行情总线：承载 [`MarketEvent`]（无账户归属，一份服务所有账户）。
pub type MarketPubSub = PubSub<MarketEvent>;

/// 账户私有事件总线：承载 [`AccountEvent`]（账户是必填结构字段）。
///
/// 实盘适配层（标 [`AccountId::Live`]）与本地柜台 `PaperCounterActor`（标 `Paper(x)`）
/// 发布**同一个类型**到这一条总线，消费者按 `account` 字段取自己的那份 ——
/// 账户隔离由类型与字段值保证，不靠总线拓扑，也不靠"来源即 Live"的推断。
pub type AccountPubSub = PubSub<AccountEvent>;

/// 带账户归属的策略信号。
///
/// 一条 OutcomePubSub 上同时跑实盘与多个模拟账户的订单：[`OutcomeProcessorActor`] 只处理
/// [`AccountId::Live`]，[`PaperCounterActor`] 只处理 `Paper(_)`。这样"发往交易所"与"落本地
/// 柜台"仍是两个互不知情的实现，且天然支持几百个模拟账户，无需为每个账户开一条总线。
#[derive(Debug, Clone)]
pub struct AccountOutcome {
    pub account: AccountId,
    pub event: OutcomeEvent,
}

/// Outcome 事件的 PubSub Actor 类型
pub type OutcomePubSub = PubSub<AccountOutcome>;

/// 一条策略信号该由哪个出口执行。
///
/// 出口本身是两个互不知情的实现（[`OutcomeProcessorActor`] 发往交易所、
/// [`PaperCounterActor`] 落本地柜台），但"这条归谁"是**一个**判断，只能有一处
/// （见 `docs/architecture.md` 原则 P4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outlet {
    /// 发往交易所（唯一实盘下单出口 `OrderGateway`）
    Live,
    /// 落本地柜台撮合
    Paper,
}

/// 分发判据的**唯一**出处：两个出口的订阅过滤器都调它。
///
/// # 为什么必须收在一处
///
/// 此前两个出口各在自己的 handler 里写否定条件 —— `if account != Live { return }` 与
/// `if !account.is_paper() { return }`。`AccountId` 只有两个 variant 时它们恰好互补，
/// 但那是巧合不是保证：**新增第三类账户时两处都不会编译失败**，结果要么静默双执行、
/// 要么静默不执行，而两者都没有外在症状。
///
/// 收成一个穷举 `match` 之后，新增 variant 会在这里编译失败（原则 P6），
/// 而"恰有一个出口接受"由 [`outlet_dispatch_tests`] 钉住。
pub const fn outlet_for(account: &AccountId) -> Outlet {
    match account {
        AccountId::Live => Outlet::Live,
        AccountId::Paper(_) => Outlet::Paper,
    }
}

/// 实盘出口的订阅过滤器（`ManagerActor` 装配时传给 `SubscribeFilter`）。
///
/// 提成具名函数而非就地写闭包，是为了让 [`outlet_dispatch_tests`] 断言的就是**装配处
/// 真正用的那一个** —— 测试里复写一遍等价表达式，证明不了线上跑的是什么。
pub fn to_live_outlet(o: &AccountOutcome) -> bool {
    matches!(outlet_for(&o.account), Outlet::Live)
}

/// 本地柜台出口的订阅过滤器，与 [`to_live_outlet`] 成对
pub fn to_paper_outlet(o: &AccountOutcome) -> bool {
    matches!(outlet_for(&o.account), Outlet::Paper)
}


pub use assembly::{setup_binance, setup_hyperliquid, setup_ibkr, setup_okx, ExchangeSetup};
pub use clock::{ClockActor, ClockActorArgs};
pub use crypto_status::{CryptoStatusActor, CryptoStatusActorArgs};
pub use manager::{AddStrategy, AddStrategies, GetAllSymbolMetas, ManagerActor, ManagerActorArgs, PublishCustomEvent, RegisterSupervisedChild, SubscribeAccount, SubscribeMarket, SubscribeOutcome, StrategySpec, RemoveStrategies};
pub use executor::{ExecutorActor, ExecutorArgs, GetPositions};
pub use income_processor::{IncomeProcessorActor, RegisterExecutor, UnregisterExecutor};
pub use metrics::{MetricsActor, MetricsActorArgs, RegisterSymbols, DEFAULT_REPORT_INTERVAL_MS};
pub use outcome_processor::{OrderGateway, OutcomeProcessorActor, PlaceVerdict};
pub use paper_counter::{PaperCounterActor, PaperCounterArgs};
pub use position_ledger::{
    GetLivePositions, PositionLedgerActor, PositionLedgerArgs, Reconciler,
    DEFAULT_MAX_CONSECUTIVE_MISMATCHES, DEFAULT_POSITION_POLL_INTERVAL_MS,
};
pub use supervisor::{
    Decision, NeverPromote, PromotionPolicy, RoundTrip, StrategyFactory, SupervisorActor,
    SupervisorArgs, SymbolRecord, SymbolView,
};

#[cfg(test)]
mod outlet_dispatch_tests {
    use super::*;

    fn signal(account: AccountId) -> AccountOutcome {
        AccountOutcome {
            account,
            event: OutcomeEvent::PlaceOrders {
                orders: Vec::new(),
                comment: String::new(),
            },
        }
    }

    /// 代表性账户样本。`Paper(String)` 的取值无穷，取几个有代表性的标签即可 ——
    /// 判据只 match variant，不看标签内容。
    fn samples() -> Vec<AccountId> {
        vec![
            AccountId::Live,
            AccountId::Paper("BTC".to_string()),
            AccountId::Paper(String::new()),
            AccountId::Paper("Live".to_string()), // 标签叫 Live 也仍是模拟账户
        ]
    }

    /// **不重不漏**：每条信号恰好被一个出口接受。
    ///
    /// 用的是装配处真正传给 `SubscribeFilter` 的那两个函数。漏 = 信号无人执行（策略发了单
    /// 却什么都没发生）；重 = 同一条单既发交易所又落柜台。两者都没有外在症状。
    #[test]
    fn every_signal_is_claimed_by_exactly_one_outlet() {
        for account in samples() {
            let s = signal(account.clone());
            let claimed = [to_live_outlet(&s), to_paper_outlet(&s)]
                .iter()
                .filter(|x| **x)
                .count();
            assert_eq!(
                claimed, 1,
                "{account} 被 {claimed} 个出口接受，应恰为 1（0 = 信号丢失，2 = 双执行）"
            );
        }
    }

    /// 归属判据只看 variant，不看标签内容
    #[test]
    fn routing_follows_the_variant_not_the_label() {
        assert_eq!(outlet_for(&AccountId::Live), Outlet::Live);
        for account in samples().into_iter().filter(|a| a.is_paper()) {
            assert_eq!(outlet_for(&account), Outlet::Paper, "{account} 应落柜台");
        }
    }
}

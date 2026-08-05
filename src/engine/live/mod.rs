//! Engine Actor 模块
//!
//! 包含引擎中各个 Actor 的实现

mod clock;
mod crypto_status;
mod manager;
mod executor;
mod income_processor;
mod metrics;
mod outcome_processor;
mod paper_counter;
mod position_polling;
mod position_reconcile;
mod supervisor;

use crate::domain::AccountId;
use crate::messaging::IncomeEvent;
use crate::strategy::OutcomeEvent;
use kameo_actors::pubsub::PubSub;

/// Income 事件的 PubSub Actor 类型。
///
/// 承载**共享行情** + **实盘账户**的私有事件（后者由各所 private WS 发布，其来源即
/// 意味着归属 [`AccountId::Live`]）。模拟账户的私有事件走 [`PaperPubSub`]。
pub type IncomePubSub = PubSub<IncomeEvent>;

/// 模拟账户私有事件的 PubSub。
///
/// 与 [`IncomePubSub`] 分开，是为了让"实盘账户的成交"在结构上不可能流进模拟策略的状态
/// （见 [`AccountId`]）。行情不走这条 —— 行情共享一份，没有账户归属。
pub type PaperPubSub = PubSub<AccountIncome>;

/// 带账户归属的私有事件
#[derive(Debug, Clone)]
pub struct AccountIncome {
    pub account: AccountId,
    pub event: IncomeEvent,
}

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

pub use clock::{ClockActor, ClockActorArgs};
pub use crypto_status::{CryptoStatusActor, CryptoStatusActorArgs};
pub use manager::{AddStrategy, AddStrategies, GetAllSymbolMetas, GetIbkrClient, PublishIncome, ManagerActor, ManagerActorArgs, Stop, SubscribeIncome, SubscribeOutcome, SubscribePaper, StrategySpec, RemoveStrategies};
pub use executor::{ExecutorActor, ExecutorArgs};
pub use income_processor::{IncomeProcessorActor, RegisterExecutor, UnregisterExecutor};
pub use metrics::{MetricsActor, MetricsActorArgs, RegisterSymbols, DEFAULT_REPORT_INTERVAL_MS};
pub use outcome_processor::{OutcomeProcessorActor, OutcomeProcessorArgs};
pub use paper_counter::{PaperCounterActor, PaperCounterArgs};
pub use position_polling::{
    PositionPollingActor, PositionPollingActorArgs, DEFAULT_POSITION_POLL_INTERVAL_MS,
};
pub use position_reconcile::{
    PositionReconcileActor, PositionReconcileArgs, Reconciler,
    DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
};
pub use supervisor::{
    Decision, NeverPromote, PromotionPolicy, RoundTrip, StrategyFactory, SupervisorActor,
    SupervisorArgs, SymbolRecord, SymbolView,
};

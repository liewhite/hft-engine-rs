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
mod position_reconcile;
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

pub use assembly::{setup_binance, setup_hyperliquid, setup_ibkr, setup_okx, ExchangeSetup};
pub use clock::{ClockActor, ClockActorArgs};
pub use crypto_status::{CryptoStatusActor, CryptoStatusActorArgs};
pub use manager::{AddStrategy, AddStrategies, GetAllSymbolMetas, ManagerActor, ManagerActorArgs, PublishCustomEvent, RegisterSupervisedChild, SubscribeAccount, SubscribeMarket, SubscribeOutcome, StrategySpec, RemoveStrategies};
pub use executor::{ExecutorActor, ExecutorArgs, GetPositions};
pub use income_processor::{IncomeProcessorActor, RegisterExecutor, UnregisterExecutor};
pub use metrics::{MetricsActor, MetricsActorArgs, RegisterSymbols, DEFAULT_REPORT_INTERVAL_MS};
pub use outcome_processor::{OrderGateway, OutcomeProcessorActor, PlaceVerdict};
pub use paper_counter::{PaperCounterActor, PaperCounterArgs};
pub use position_reconcile::{
    PositionReconcileActor, PositionReconcileArgs, Reconciler,
    DEFAULT_MAX_CONSECUTIVE_MISMATCHES, DEFAULT_POSITION_POLL_INTERVAL_MS,
};
pub use supervisor::{
    Decision, NeverPromote, PromotionPolicy, RoundTrip, StrategyFactory, SupervisorActor,
    SupervisorArgs, SymbolRecord, SymbolView,
};

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

use crate::messaging::IncomeEvent;
use crate::strategy::OutcomeEvent;
use kameo_actors::pubsub::PubSub;

/// Income 事件的 PubSub Actor 类型
pub type IncomePubSub = PubSub<IncomeEvent>;

/// Outcome 事件的 PubSub Actor 类型
pub type OutcomePubSub = PubSub<OutcomeEvent>;

pub use clock::{ClockActor, ClockActorArgs};
pub use crypto_status::{CryptoStatusActor, CryptoStatusActorArgs};
pub use manager::{AddStrategy, AddStrategies, GetAllSymbolMetas, GetIbkrClient, PublishIncome, ManagerActor, ManagerActorArgs, RegisterMetricsSymbols, Stop, SubscribeIncome, SubscribeOutcome};
pub use executor::{ExecutorActor, ExecutorArgs};
pub use income_processor::{IncomeProcessorActor, RegisterExecutor};
pub use metrics::{
    symbol_exposure, MetricsActor, MetricsActorArgs, RegisterSymbols, SymbolExposure,
    DEFAULT_REPORT_INTERVAL_MS,
};
pub use outcome_processor::{OutcomeProcessorActor, OutcomeProcessorArgs};

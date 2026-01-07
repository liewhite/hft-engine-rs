//! Engine Actor 模块
//!
//! 包含引擎中各个 Actor 的实现

mod clock;
mod manager;
mod executor;
mod processor;
mod signal_processor;

use crate::messaging::IncomeEvent;
use crate::strategy::OutcomeEvent;
use kameo_actors::pubsub::PubSub;

/// Income 事件的 PubSub Actor 类型
pub type IncomePubSub = PubSub<IncomeEvent>;

/// Outcome 事件的 PubSub Actor 类型
pub type OutcomePubSub = PubSub<OutcomeEvent>;

pub use clock::{ClockActor, ClockActorArgs};
pub use manager::{AddStrategy, ManagerActor, ManagerActorArgs, Stop, SubscribeIncome, SubscribeOutcome};
pub use executor::{ExecutorActor, ExecutorArgs};
pub use processor::{IncomeProcessorActor, RegisterExecutor};
pub use signal_processor::{OutcomeProcessorActor, SignalProcessorArgs};

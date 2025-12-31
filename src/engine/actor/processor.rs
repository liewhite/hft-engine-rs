//! ProcessorActor - 事件分发 Actor
//!
//! 职责：
//! - 接收来自 ExchangeActor 的 ExchangeEvent
//! - 根据订阅关系分发到对应的 ExecutorActor

use super::ExecutorActor;
use crate::domain::{Exchange, Symbol};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};

/// 事件路由类型
enum EventRouting {
    /// 按 (exchange, symbol) 路由到订阅了该 symbol 的 executor
    BySymbol { exchange: Exchange, symbol: Symbol },
    /// 广播给所有 executor
    Broadcast,
}

/// Executor 订阅信息
struct ExecutorSubscription {
    executor: ActorRef<ExecutorActor>,
    /// 订阅的 (exchange, symbol) 集合
    symbols: HashSet<(Exchange, Symbol)>,
}

/// ProcessorActor - 事件分发
pub struct ProcessorActor {
    /// ActorID -> Executor 订阅信息
    executors: HashMap<ActorID, ExecutorSubscription>,
}

impl ProcessorActor {
    pub fn new() -> Self {
        Self {
            executors: HashMap::new(),
        }
    }

    /// 确定事件的路由方式
    fn event_routing(event: &IncomeEvent) -> EventRouting {
        match &event.data {
            // Public 数据：按 (exchange, symbol) 路由
            ExchangeEventData::FundingRate(rate) => EventRouting::BySymbol {
                exchange: rate.exchange,
                symbol: rate.symbol.clone(),
            },
            ExchangeEventData::BBO(bbo) => EventRouting::BySymbol {
                exchange: bbo.exchange,
                symbol: bbo.symbol.clone(),
            },
            // Private 数据：Position 和 OrderUpdate 按 symbol 路由
            ExchangeEventData::Position(pos) => EventRouting::BySymbol {
                exchange: pos.exchange,
                symbol: pos.symbol.clone(),
            },
            ExchangeEventData::OrderUpdate(update) => EventRouting::BySymbol {
                exchange: update.exchange,
                symbol: update.symbol.clone(),
            },
            // 账户级别数据和 Clock：广播
            ExchangeEventData::Balance(_)
            | ExchangeEventData::Equity { .. }
            | ExchangeEventData::Clock => EventRouting::Broadcast,
        }
    }
}

impl Default for ProcessorActor {
    fn default() -> Self {
        Self::new()
    }
}

impl Actor for ProcessorActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ProcessorActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::info!("ProcessorActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("ProcessorActor stopped");
        Ok(())
    }
}

// ============================================================================
// Messages
// ============================================================================

/// 注册 Executor 及其订阅
pub struct RegisterExecutor {
    pub executor: ActorRef<ExecutorActor>,
    pub subscriptions: HashSet<(Exchange, SubscriptionKind)>,
}

impl Message<RegisterExecutor> for ProcessorActor {
    type Reply = ();

    async fn handle(&mut self, msg: RegisterExecutor, _ctx: Context<'_, Self, Self::Reply>) {
        let actor_id = msg.executor.id();

        // 从 subscriptions 提取 (exchange, symbol) 集合
        let symbols: HashSet<(Exchange, Symbol)> = msg
            .subscriptions
            .iter()
            .map(|(ex, kind)| (*ex, kind.symbol().clone()))
            .collect();

        tracing::info!(
            executor_id = ?actor_id,
            symbols = ?symbols,
            "Executor registered"
        );

        self.executors.insert(
            actor_id,
            ExecutorSubscription {
                executor: msg.executor,
                symbols,
            },
        );
    }
}

/// ExchangeEvent 消息 - 从 ExchangeActor 接收
impl Message<IncomeEvent> for ProcessorActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: Context<'_, Self, Self::Reply>) {
        match Self::event_routing(&msg) {
            EventRouting::BySymbol { exchange, symbol } => {
                // 按 symbol 路由：只发送给订阅了该 (exchange, symbol) 的 executor
                for sub in self.executors.values() {
                    if sub.symbols.contains(&(exchange, symbol.clone())) {
                        let _ = sub.executor.tell(msg.clone()).await;
                    }
                }
            }
            EventRouting::Broadcast => {
                // 广播给所有 executor
                for sub in self.executors.values() {
                    let _ = sub.executor.tell(msg.clone()).await;
                }
            }
        }
    }
}

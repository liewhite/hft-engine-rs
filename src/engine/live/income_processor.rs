//! ProcessorActor - 事件分发 Actor
//!
//! 职责：
//! - 订阅 IncomePubSub 接收事件
//! - 根据订阅关系分发到对应的 ExecutorActor

use super::ExecutorActor;
use crate::domain::{Exchange, Symbol};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorId, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
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
pub struct IncomeProcessorActor {
    /// ActorId -> Executor 订阅信息
    executors: HashMap<ActorId, ExecutorSubscription>,
}

impl IncomeProcessorActor {
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
            ExchangeEventData::MarkPrice(mp) => EventRouting::BySymbol {
                exchange: mp.exchange,
                symbol: mp.symbol.clone(),
            },
            ExchangeEventData::IndexPrice(ip) => EventRouting::BySymbol {
                exchange: ip.exchange,
                symbol: ip.symbol.clone(),
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
            | ExchangeEventData::AccountInfo { .. }
            | ExchangeEventData::Clock => EventRouting::Broadcast,
        }
    }
}

impl Default for IncomeProcessorActor {
    fn default() -> Self {
        Self {
            executors: HashMap::new(),
        }
    }
}

impl Actor for IncomeProcessorActor {
    type Args = Self;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::info!("ProcessorActor started");
        Ok(args)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
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

impl Message<RegisterExecutor> for IncomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RegisterExecutor,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
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

/// IncomeEvent 消息 - 从 IncomePubSub 接收
impl Message<IncomeEvent> for IncomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: IncomeEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match Self::event_routing(&msg) {
            EventRouting::BySymbol { exchange, symbol } => {
                // 按 symbol 路由：只发送给订阅了该 (exchange, symbol) 的 executor
                for sub in self.executors.values() {
                    if sub.symbols.contains(&(exchange, symbol.clone())) {
                        let _ = sub.executor.tell(msg.clone()).send().await;
                    }
                }
            }
            EventRouting::Broadcast => {
                // 广播给所有 executor
                for sub in self.executors.values() {
                    let _ = sub.executor.tell(msg.clone()).send().await;
                }
            }
        }
    }
}

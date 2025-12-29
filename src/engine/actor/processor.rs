//! ProcessorActor - 市场数据分发 Actor
//!
//! 职责：
//! - 接收来自 ExchangeActor 的 MarketData
//! - 根据订阅关系分发到对应的 ExecutorActor

use super::ExecutorActor;
use crate::domain::Exchange;
use crate::exchange::{MarketData, SubscriptionKind};
use kameo::actor::{ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};

/// Executor 订阅信息
struct ExecutorSubscription {
    executor: ActorRef<ExecutorActor>,
    subscriptions: HashSet<(Exchange, SubscriptionKind)>,
}

/// ProcessorActor - 市场数据分发
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

    /// 将 MarketData 转换为 SubscriptionKind
    fn market_data_to_key(data: &MarketData) -> Option<(Exchange, SubscriptionKind)> {
        match data {
            MarketData::FundingRate { exchange, symbol, .. } => {
                Some((*exchange, SubscriptionKind::FundingRate { symbol: symbol.clone() }))
            }
            MarketData::BBO { exchange, symbol, .. } => {
                Some((*exchange, SubscriptionKind::BBO { symbol: symbol.clone() }))
            }
            // Private 数据广播给所有 executor
            MarketData::Position { .. }
            | MarketData::Balance { .. }
            | MarketData::OrderUpdate { .. }
            | MarketData::Equity { .. } => None,
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
        tracing::info!(
            executor_id = ?actor_id,
            subscriptions = msg.subscriptions.len(),
            "Executor registered"
        );
        self.executors.insert(actor_id, ExecutorSubscription {
            executor: msg.executor,
            subscriptions: msg.subscriptions,
        });
    }
}

/// MarketData 消息 - 从 ExchangeActor 接收
impl Message<MarketData> for ProcessorActor {
    type Reply = ();

    async fn handle(&mut self, msg: MarketData, _ctx: Context<'_, Self, Self::Reply>) {
        let key = Self::market_data_to_key(&msg);

        match key {
            Some(key) => {
                // Public 数据：只发送给订阅了该数据的 executor
                for sub in self.executors.values() {
                    if sub.subscriptions.contains(&key) {
                        let _ = sub.executor.tell(msg.clone()).await;
                    }
                }
            }
            None => {
                // Private 数据：广播给所有 executor
                for sub in self.executors.values() {
                    let _ = sub.executor.tell(msg.clone()).await;
                }
            }
        }
    }
}

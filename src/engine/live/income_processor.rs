//! ProcessorActor - 事件分发 Actor
//!
//! 职责：
//! - 订阅 IncomePubSub 接收事件
//! - 根据订阅关系分发到对应的 ExecutorActor

use super::{AccountIncome, ExecutorActor};
use crate::domain::{AccountId, Exchange, Symbol};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorId, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};

/// Executor 订阅信息
struct ExecutorSubscription {
    executor: ActorRef<ExecutorActor>,
    /// 订阅的 (exchange, symbol) 集合
    symbols: HashSet<(Exchange, Symbol)>,
    /// 该 executor 绑定的账户：**私有事件只投给账户匹配者**
    account: AccountId,
}

/// 该事件是否属于某个账户私有（而非共享行情）。
///
/// 私有事件只对**一个账户**有意义：把实盘的成交投给模拟策略，会让它把别人的仓位记成自己的。
/// 行情相反 —— 一份数据服务所有账户。
fn is_account_private(event: &IncomeEvent) -> bool {
    matches!(
        event.data,
        ExchangeEventData::OrderUpdate(_)
            | ExchangeEventData::Fill(_)
            | ExchangeEventData::Balance(_)
            | ExchangeEventData::AccountInfo { .. }
            | ExchangeEventData::FundingFee(_)
    )
}

/// ProcessorActor - 事件分发
pub struct IncomeProcessorActor {
    /// ActorId -> Executor 订阅信息
    executors: HashMap<ActorId, ExecutorSubscription>,
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
    /// 该 executor 绑定的账户
    pub account: AccountId,
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
                account: msg.account,
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
        // 共享总线上的私有事件来自交易所的 private WS / REST，即**实盘账户**；模拟账户的
        // 私有事件走 PaperPubSub（见 Message<AccountIncome>）。这不是约定，而是来源决定的事实。
        let private_owner = is_account_private(&msg).then_some(AccountId::Live);
        match msg.routing() {
            // 按 symbol 路由：只发送给订阅了该 (exchange, symbol) 的 executor
            Some((exchange, symbol)) => {
                for sub in self.executors.values() {
                    if private_owner.as_ref().is_some_and(|o| &sub.account != o) {
                        continue;
                    }
                    if sub.symbols.contains(&(exchange, symbol.clone())) {
                        if let Err(e) = sub.executor.tell(msg.clone()).send().await {
                            tracing::error!(error = %e, "Failed to forward event to executor");
                        }
                    }
                }
            }
            // 广播给所有 executor（账户私有的广播事件如 Balance/AccountInfo 仍按账户过滤）
            None => {
                for sub in self.executors.values() {
                    if private_owner.as_ref().is_some_and(|o| &sub.account != o) {
                        continue;
                    }
                    if let Err(e) = sub.executor.tell(msg.clone()).send().await {
                        tracing::error!(error = %e, "Failed to forward event to executor");
                    }
                }
            }
        }
    }
}

/// 模拟账户的私有事件：只投给**该账户**的 executor。
///
/// 与共享总线的处理构成对称：那边的私有事件必属实盘账户，这边的必属某个模拟账户。
/// 行情不走这条 —— 行情共享一份，由 [`Message<IncomeEvent>`] 分发给所有账户。
impl Message<AccountIncome> for IncomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: AccountIncome,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if !is_account_private(&msg.event) {
            tracing::error!(
                account = %msg.account,
                "PaperPubSub 上出现了非私有事件（行情应走共享总线），已忽略"
            );
            return;
        }
        for sub in self.executors.values() {
            if sub.account != msg.account {
                continue;
            }
            if let Err(e) = sub.executor.tell(msg.event.clone()).send().await {
                tracing::error!(error = %e, "Failed to forward paper event to executor");
            }
        }
    }
}

/// 注销 executor（动态撤下策略实例时使用）
pub struct UnregisterExecutor {
    pub executor: ActorRef<ExecutorActor>,
}

impl Message<UnregisterExecutor> for IncomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: UnregisterExecutor,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let id = msg.executor.id();
        if self.executors.remove(&id).is_none() {
            tracing::warn!(?id, "UnregisterExecutor: 该 executor 未注册，忽略");
        }
    }
}

#[cfg(test)]
mod account_isolation_tests {
    use super::*;
    use crate::domain::{Exchange, Fill, FillReason, MarketTrade, Side};

    fn fill_event() -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: ExchangeEventData::Fill(Fill {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                side: Side::Long,
                price: 100.0,
                size: 1.0,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: 1,
                fee: 0.0,
                reason: FillReason::Normal,
            }),
        }
    }

    fn trade_event() -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: ExchangeEventData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                price: 100.0,
                qty: 1.0,
                is_buyer_maker: false,
                timestamp: 1,
            }),
        }
    }

    // 注：历史上这里有"PositionBaseline / PositionReport 绝不路由给 executor"的守恒测试。
    // 两者已退出事件枚举（基线是投产握手载荷、读数由对账 actor 自行轮询），这条不变量
    // 从"测试守着的约定"升级成了"类型上不可表达"—— 测试随之删除。

    /// 成交/持仓/净值属于某一个账户；行情不属于任何账户。
    /// 这条分类是账户隔离的依据 —— 分错一类，别人的仓位就会被记进自己的状态。
    #[test]
    fn account_private_events_are_classified_correctly() {
        assert!(is_account_private(&fill_event()), "成交是账户私有");
        assert!(!is_account_private(&trade_event()), "公共成交印记是行情，不是私有");

        let clock = IncomeEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: ExchangeEventData::Clock,
        };
        assert!(!is_account_private(&clock), "Clock 对所有账户可见");
    }
}

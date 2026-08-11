//! IncomeProcessorActor - 事件分发 Actor
//!
//! 订阅 MarketBus 与 AccountBus，按订阅关系与账户归属分发到对应的 ExecutorActor：
//! - 行情（[`MarketEvent`]）：一份服务所有账户 —— 有 symbol 的按 (exchange, symbol)
//!   定向投给订阅者，无 symbol 的（Clock/ExchangeStatus/无 scope 的 Custom）广播；
//! - 账户事件（[`AccountEvent`]）：只投给**该账户**的 executor —— 有 symbol 的再按
//!   订阅过滤，账户级的（Balance/AccountInfo/Greeks）投给该账户全部 executor。
//!
//! 账户归属由事件自带的 `account` 字段决定（类型保证），不存在需要维护的分类表 ——
//! 历史上这里有一张手工的 `is_account_private` 变体清单，新增私有变体漏改一行的
//! 失效方向是"实盘私有事件广播给模拟策略"（危险侧、编译器不报）。
//!
//! # 路由索引
//!
//! 分发是热路径（每条 BBO 都经过），按 (exchange, symbol) 与账户建反向索引做 O(1)
//! 查找 —— 此前对每条事件线性扫描全部 executor，数百 symbol 的部署下每个行情 tick
//! 都是 O(n) 扫描。

use super::ExecutorActor;
use crate::domain::{AccountId, Exchange, Symbol};
use crate::exchange::SubscriptionKind;
use crate::messaging::{AccountEvent, IncomeEvent, MarketEvent};
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
    /// 该 executor 绑定的账户
    account: AccountId,
}

/// ProcessorActor - 事件分发
#[derive(Default)]
pub struct IncomeProcessorActor {
    /// ActorId -> Executor 订阅信息
    executors: HashMap<ActorId, ExecutorSubscription>,
    /// 反向索引：(exchange, symbol) -> 订阅者（热路径 O(1) 查找）
    by_symbol: HashMap<(Exchange, Symbol), Vec<ActorId>>,
    /// 反向索引：账户 -> 该账户的全部 executor
    by_account: HashMap<AccountId, Vec<ActorId>>,
}

impl IncomeProcessorActor {
    fn remove_from_indexes(&mut self, id: ActorId) {
        if let Some(sub) = self.executors.remove(&id) {
            for key in &sub.symbols {
                if let Some(v) = self.by_symbol.get_mut(key) {
                    v.retain(|x| *x != id);
                    if v.is_empty() {
                        self.by_symbol.remove(key);
                    }
                }
            }
            if let Some(v) = self.by_account.get_mut(&sub.account) {
                v.retain(|x| *x != id);
                if v.is_empty() {
                    self.by_account.remove(&sub.account);
                }
            }
        }
    }

    async fn forward(sub: &ExecutorSubscription, event: &IncomeEvent) {
        if let Err(e) = sub.executor.tell(event.clone()).send().await {
            tracing::error!(error = %e, "Failed to forward event to executor");
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
        // 幂等：同一 executor 重复注册时先摘净旧索引 —— 否则 by_symbol/by_account 里
        // 出现重复 id，每条事件双份投递、Fill 双计仓位（危险侧、静默）。今天注册点单一
        // 且 ActorId 唯一，但这不该是巧合保护，而是结构保证。
        self.remove_from_indexes(actor_id);

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

        for key in &symbols {
            self.by_symbol.entry(key.clone()).or_default().push(actor_id);
        }
        self.by_account
            .entry(msg.account.clone())
            .or_default()
            .push(actor_id);
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

/// 行情事件：一份服务所有账户。有 symbol 的定向投给订阅者，无 symbol 的广播。
impl Message<MarketEvent> for IncomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: MarketEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let event = IncomeEvent::Market(msg);
        match event.routing() {
            Some(key) => {
                // 定向：索引 O(1)
                if let Some(ids) = self.by_symbol.get(&key) {
                    for id in ids {
                        if let Some(sub) = self.executors.get(id) {
                            Self::forward(sub, &event).await;
                        }
                    }
                }
            }
            None => {
                // 广播（Clock / ExchangeStatus / 无 scope 的 Custom）
                for sub in self.executors.values() {
                    Self::forward(sub, &event).await;
                }
            }
        }
    }
}

/// 账户事件：只投给**该账户**的 executor（账户由事件自带标签决定，类型保证）。
/// 有 symbol 的再按订阅过滤，账户级的投给该账户全部 executor。
impl Message<AccountEvent> for IncomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: AccountEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let account = msg.account.clone();
        let event = IncomeEvent::Account(msg);
        let Some(ids) = self.by_account.get(&account) else {
            return;
        };
        let routing = event.routing();
        for id in ids {
            let Some(sub) = self.executors.get(id) else {
                continue;
            };
            if let Some(key) = &routing {
                if !sub.symbols.contains(key) {
                    // 该账户的私有事件、但 executor 没订这个 symbol。
                    // - Live：分桶部署的常态（账户级全量私有流含其他桶的 symbol），静默跳过；
                    // - Paper：柜台只为本进程策略的订单产回报，被过滤即异常 —— 策略对未订阅
                    //   symbol 下了单，回报回不来，订单会停在 Created、超时清理后反复重挂。
                    //   丢弃点必须可见。
                    if account != AccountId::Live {
                        tracing::warn!(
                            %account,
                            exchange = %key.0,
                            symbol = %key.1,
                            "模拟账户的私有回报被 symbol 订阅过滤丢弃 —— 策略在未订阅的 symbol 上下了单？"
                        );
                    }
                    continue;
                }
            }
            Self::forward(sub, &event).await;
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
        if self.executors.contains_key(&id) {
            self.remove_from_indexes(id);
        } else {
            tracing::warn!(?id, "UnregisterExecutor: 该 executor 未注册，忽略");
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::domain::{Exchange, Fill, FillReason, MarketTrade, Side};
    use crate::messaging::{AccountData, MarketData};

    fn fill_event(account: AccountId) -> AccountEvent {
        AccountEvent {
            account,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Fill(Fill {
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

    fn trade_event() -> MarketEvent {
        MarketEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: MarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                price: 100.0,
                qty: 1.0,
                is_buyer_maker: false,
                timestamp: 1,
            }),
        }
    }

    /// 账户隔离的核心：账户事件的归属是**结构字段**，路由按它分道 —— "实盘私有事件
    /// 广播给模拟策略"在类型上已不可表达，这里钉住路由推导本身。
    #[test]
    fn account_event_carries_its_owner_and_routes_by_symbol() {
        let live = fill_event(AccountId::Live);
        assert_eq!(live.account, AccountId::Live);
        let ev = IncomeEvent::Account(live);
        assert_eq!(
            ev.routing(),
            Some((Exchange::Binance, "BTC".to_string())),
            "Fill 有 symbol，应定向路由"
        );
    }

    /// 行情无账户归属，按 symbol 定向；Clock 无 symbol，广播
    #[test]
    fn market_events_route_by_symbol_or_broadcast() {
        let ev = IncomeEvent::Market(trade_event());
        assert_eq!(ev.routing(), Some((Exchange::Binance, "BTC".to_string())));

        let clock = IncomeEvent::market(0, 0, MarketData::Clock);
        assert_eq!(clock.routing(), None, "Clock 对所有账户广播");
    }
}

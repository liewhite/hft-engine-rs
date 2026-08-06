//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 IncomeEvent，委托 [`StrategyRunner`] 运行策略 (更新状态 + 转换订单)，
//! 把产出的 OutcomeEvent 发布到 OutcomePubSub。纯策略逻辑集中在 StrategyRunner，
//! 与回测引擎共享同一份实现。

use crate::domain::{AccountId, Exchange, Symbol, SymbolMeta};
use crate::engine::StrategyRunner;
use crate::messaging::IncomeEvent;
use crate::strategy::Strategy;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;
use kameo_actors::pubsub::Publish;

use super::{AccountOutcome, OutcomePubSub};

/// ExecutorActor 初始化参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// 本实例绑定的账户：决定订单落到实盘还是某个模拟账户
    pub account: AccountId,
    /// Symbol 元数据 (供 StrategyRunner 按交易所精度取整；单位折算不在此)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// Outcome PubSub 引用 (用于发布信号)
    pub outcome_pubsub: ActorRef<OutcomePubSub>,
}

/// ExecutorActor - 执行策略的 Actor
pub struct ExecutorActor {
    /// 策略执行纯逻辑核心 (状态管理 + 订单转换)
    runner: StrategyRunner,
    /// 本实例绑定的账户
    account: AccountId,
    /// Outcome PubSub 引用 (用于发布信号)
    outcome_pubsub: ActorRef<OutcomePubSub>,
}

impl ExecutorActor {
    /// 处理 IncomeEvent，委托 runner 运行策略并发布返回的信号
    async fn handle_event(&mut self, event: IncomeEvent) {
        for signal in self.runner.on_event(&event) {
            let tagged = AccountOutcome {
                account: self.account.clone(),
                event: signal,
            };
            // 投递失败 = 这条策略信号（下单/撤单/自定义事件）没有任何人收到。总线挂掉
            // 本就会经 on_link_died 整机退出，这里不重试也不吞 —— 但必须留下记录，
            // 否则"策略明明发了单却什么都没发生"无从查起。
            if let Err(e) = self.outcome_pubsub.tell(Publish(tagged)).send().await {
                tracing::error!(error = %e, account = %self.account, "策略信号发布失败，该信号已丢失");
            }
        }
    }
}

impl Actor for ExecutorActor {
    type Args = ExecutorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let runner = StrategyRunner::new(args.strategy, args.symbol_metas);
        let account = args.account;
        tracing::info!("ExecutorActor started");
        Ok(Self {
            runner,
            account,
            outcome_pubsub: args.outcome_pubsub,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("ExecutorActor stopped");
        Ok(())
    }
}

// === Messages ===

/// IncomeEvent 消息 - 从 ProcessorActor 接收 (包含所有事件类型，含 Clock)
impl Message<IncomeEvent> for ExecutorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: IncomeEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_event(msg).await;
    }
}

/// 查询本实例维护的持仓（指定 symbol，返回其在各所的仓位）。
///
/// 用于降级平仓取量，见 [`crate::engine::RemoveStrategies`]。读的是「基线 + Fill」维护的
/// **权威**持仓，**不打 REST**：
/// - REST 在本项目里只作对账用途（见 [`crate::engine::PositionReconcileActor`]），
///   持仓维护模型不允许它参与；
/// - 本地值比 REST 快照更**新** —— 后者可能落后于最新到达的 Fill。
///
/// 调用方必须在 `kill` 该 executor **之前**发起查询：kill 之后它就不再消费事件、状态停更。
pub struct GetPositions(pub Vec<Symbol>);

impl Message<GetPositions> for ExecutorActor {
    type Reply = Vec<crate::domain::Position>;

    async fn handle(
        &mut self,
        msg: GetPositions,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let state = self.runner.state();
        msg.0
            .iter()
            .filter_map(|symbol| state.symbol_state(symbol))
            .flat_map(|symbol_state| symbol_state.positions.values().cloned())
            .collect()
    }
}

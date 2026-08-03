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
            let _ = self.outcome_pubsub.tell(Publish(tagged)).send().await;
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

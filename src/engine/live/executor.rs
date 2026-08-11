//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 IncomeEvent，委托 [`StrategyRunner`] 运行策略 (更新状态 + 转换订单)，
//! 把产出的 OutcomeEvent 发布到 OutcomePubSub。纯策略逻辑集中在 StrategyRunner，
//! 与回测引擎共享同一份实现。

use crate::domain::{now_ms, AccountId, Exchange, Symbol, SymbolMeta};
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

/// 策略消费一条事件时，距适配层收到它的最大容忍时长（见
/// [`ExecutorActor::check_pipeline_lag`]）。
///
/// 取 1 秒：正常路径上这个值是几毫秒（三跳进程内邮箱），到秒级只有一个解释 —— 本实例
/// 跟不上事件流。它同时是策略决策的陈旧度上限：spread_arb 的行情新鲜度门是秒级，
/// 拿超过 1 秒的报价去算价差已经没有意义。
const MAX_PIPELINE_LAG_MS: u64 = 1_000;

/// 积压时每多少条超限事件报一次（首条即报，之后按整倍数节流）。
///
/// 积压场景下超限事件成千上万，逐条打日志会让告警本身变成负载。
const LAG_WARN_EVERY: u64 = 1_000;

/// ExecutorActor 初始化参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// 本实例绑定的账户：决定订单落到实盘还是某个模拟账户
    pub account: AccountId,
    /// Symbol 元数据 (供 StrategyRunner 按交易所精度取整；单位折算不在此)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 持仓基线（投产握手）：实例**出生即带基线**，在收到任何流事件之前写入 ——
    /// "Fill 早于基线、存量被丢"的竞态在结构上不可能。模拟账户从 0 起算，传空。
    pub baselines: Vec<crate::messaging::PositionBaseline>,
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
    /// 累计消费到的超限滞后事件数（节流告警用，见 [`Self::check_pipeline_lag`]）
    lagging_events: u64,
}

impl ExecutorActor {
    /// 处理 IncomeEvent，委托 runner 运行策略并发布返回的信号
    async fn handle_event(&mut self, event: IncomeEvent) {
        self.check_pipeline_lag(&event);
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

    /// 管线滞后自查：策略此刻拿到的这条事件，距适配层收到它已经过了多久。
    ///
    /// # 为什么需要它，以及为什么不是 conflation
    ///
    /// 三条总线都是 unbounded 邮箱 + `BestEffort`。消费者跟不上时，后果不是丢事件而是
    /// **积压**：策略排着队消费几秒前的报价做决策，而这件事目前**完全不可见** —— 没有
    /// 日志、没有指标，只有内存悄悄涨。
    ///
    /// 计划里原本想用"状态类事件只保留最新值"（conflation）来解决。**不能这么做**：
    /// `spread_arb` 把 BBO 当**流**消费 —— `EmaCalculator` 每条 BBO 更新一次、
    /// `is_ready()` 数的是 tick 次数。丢掉中间 tick 会让 EMA 的值与预热时机变成**负载的
    /// 函数**：同一段行情，机器忙时和闲时算出不同的 EMA，且与逐 tick 回放的回测必然背离
    /// —— 那正是"回测调好的参数上线即漂移"，本次重构阶段 1 刚花力气消除的东西。
    ///
    /// 所以改为让积压**可见**（与 [`crate::exchange::staleness`] 同一姿态：读数流的问题
    /// 要能被发现，而不是被静默吸收）：超过阈值即 warn，`ALERT_WEBHOOK_URL` 会把 WARN
    /// 外送成告警（见 [`crate::engine::init_tracing`]）。
    ///
    /// 不 kill：偶发的 GC / 调度抖动不该打死引擎，而持续积压会持续告警。
    ///
    /// # 只在实盘路径
    ///
    /// 本 actor 只用于实盘/模拟盘（回测直接驱动 `StrategyRunner`），故 `local_ts` 必是
    /// 墙钟；回测里它是虚拟时间，拿来减 `now_ms()` 会得到无意义的巨值 —— 这也是判据
    /// 放在这里、而不是放进两条驱动共享的 `StrategyRunner` 的原因。
    fn check_pipeline_lag(&mut self, event: &IncomeEvent) {
        let lag_ms = now_ms().saturating_sub(event.local_ts());
        if lag_ms <= MAX_PIPELINE_LAG_MS {
            return;
        }
        // 按整倍数节流：积压时事件本就成千上万，逐条打日志只会让告警自己变成负载
        self.lagging_events += 1;
        if self.lagging_events % LAG_WARN_EVERY == 1 {
            tracing::warn!(
                account = %self.account,
                lag_ms,
                threshold_ms = MAX_PIPELINE_LAG_MS,
                lagging_events = self.lagging_events,
                "策略正在消费积压事件（决策依据已经陈旧），本实例跟不上事件流"
            );
        }
    }
}

impl Actor for ExecutorActor {
    type Args = ExecutorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let mut runner = StrategyRunner::new(args.strategy, args.symbol_metas);
        // 基线先于一切流事件写入（on_start 完成前邮箱消息不会被处理）
        runner.seed_positions(&args.baselines);
        let account = args.account;
        tracing::info!(baselines = args.baselines.len(), "ExecutorActor started");
        Ok(Self {
            runner,
            account,
            outcome_pubsub: args.outcome_pubsub,
            lagging_events: 0,
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

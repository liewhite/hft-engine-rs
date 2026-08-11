//! ManagerActor - 顶层 Actor：监督根 + 停机链根 + 三条总线的持有者。
//!
//! 职责边界（每块各居其位，manager 只做粘合）：
//! - **装配**：收下 bin 侧的 [`ExchangeSetup`]（见 [`super::assembly`]），预加载 metas、
//!   起三条总线与全部核心子 actor —— 对"有哪些交易所"彻底无知；
//! - **投产/撤下编排**：见 [`provisioning`] 子模块（同一 actor 的方法，经邮箱串行化）；
//! - **监督**：子 actor 主动退出放行、出错级联整机退出（见 `on_link_died`）；
//! - **停机**：先停生产端、再停管线（见 `on_stop` 与 `crate::actor_lifecycle`）。

use crate::actor_lifecycle::{ChildGroup, ChildStop};
use crate::engine::bootstrap::Supervised;
use super::{
    AccountOutcome, AccountPubSub, ClockActor, ClockActorArgs, ExecutorActor,
    IncomeProcessorActor, MarketPubSub, MetricsActor, MetricsActorArgs, OrderGateway,
    OutcomePubSub, OutcomeProcessorActor, PaperCounterActor, PaperCounterArgs,
    PositionReconcileActor, PositionReconcileArgs,
    DEFAULT_MAX_CONSECUTIVE_MISMATCHES, DEFAULT_POSITION_POLL_INTERVAL_MS,
    DEFAULT_REPORT_INTERVAL_MS,
};
use crate::domain::{
    now_ms, AccountId, Exchange, ExchangeError, Symbol, SymbolMeta,
};
use crate::messaging::{AccountData, AccountEvent, MarketData, MarketEvent};
use super::assembly::{ExchangeSetup, SpawnCtx};
use crate::exchange::{ExchangeActorOps, ExchangeClient, SubscriptionKind};
use crate::strategy::Strategy;
use kameo::actor::{ActorId, ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::{Subscribe, SubscribeFilter};
use kameo_actors::DeliveryStrategy;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;

mod provisioning;

pub use provisioning::RemoveStrategies;

/// ManagerActor 初始化参数
pub struct ManagerActorArgs {
    /// 参与的交易所（在 bin 侧用各所的 `setup_*` 装配，见 [`crate::engine::assembly`]）。
    /// manager 对"有哪些所"彻底无知 —— 加新交易所零改 manager。
    pub exchanges: Vec<ExchangeSetup>,
    /// 本地柜台（模拟账户）的撮合与账本配置。
    ///
    /// 实盘与模拟**并行**：两个 outcome 消费者常驻，各按账户取自己的那份
    /// （见 [`crate::domain::AccountId`]）。因此不再有"整个进程的运行模式"这一概念 ——
    /// 一个 symbol 上完全可以模拟先跑、出信号后再拉起实盘，两者互不可见。
    pub paper: crate::sim::SimConfig,
}

/// ManagerActor - 顶层管理 Actor
pub struct ManagerActor {
    // === Symbol Metas 缓存 ===
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,

    // === Exchange Clients (REST) ===
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,

    /// 各所的订阅能力查询（知识在适配层，随 ExchangeSetup 携带；投产校验用）
    capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool>,

    // === PubSub Actors ===
    /// 公共行情总线
    market_pubsub: ActorRef<MarketPubSub>,
    /// 账户私有事件总线（实盘适配层 + 本地柜台共用，见 [`AccountPubSub`]）
    account_pubsub: ActorRef<AccountPubSub>,
    /// Outcome PubSub (策略信号)
    outcome_pubsub: ActorRef<OutcomePubSub>,

    // === 子 Actors ===
    /// ProcessorActor (订阅两条入向总线)
    income_processor: ActorRef<IncomeProcessorActor>,
    /// MetricsActor (订阅两条入向总线，输出账户/持仓/订单/历史指标)
    metrics: ActorRef<MetricsActor>,
    /// 持仓对账 Actor (订阅账户总线的实盘 Fill；漂移确认后致命退出)
    position_reconciler: ActorRef<PositionReconcileActor>,
    /// ExchangeActors (启动时创建，类型擦除)
    exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>>,

    /// 唯一的实盘下单出口（与 OutcomeProcessorActor 共享同一实例）。
    /// 降级平仓经此下单 —— in_flight 判据、失败反馈、dry_run 语义与策略信号一致。
    order_gateway: Arc<OrderGateway>,

    /// 已注册的策略实例，供动态撤下使用。
    ///
    /// 晋升/降级要求能在运行期增删实例（见 [`RemoveStrategies`]），故必须留存引用；
    /// 否则实例只能随进程生死。
    executors: Vec<RegisteredExecutor>,

    /// **产生**事件的子 actor：交易所 actor、时钟、对账轮询、策略实例。
    /// 停机时先停这一段 —— 它们的 `on_stop` 还会往总线上发最后一批事件
    /// （`IbkrPublicWsActor` 要补发等 commission 的成交），那时管线必须还活着。
    producers: ChildGroup,

    /// **传递与消费**事件的子 actor：两条 PubSub、两个 Processor、模拟柜台、
    /// 观测与对账、以及外部注册的观察者。在生产者全部停完之后才停，
    /// 这样最后一批事件仍有人接收（`Signal::Stop` 排在消息之后，会先排空）。
    ///
    /// 组内顺序同样有讲究：PubSub 先停（排空自己的队列、转发给订阅者），
    /// 订阅者后停（再排空自己的）。登记顺序即停机顺序。
    pipeline: ChildGroup,

}

/// 一个已注册的策略实例
struct RegisteredExecutor {
    executor: ActorRef<ExecutorActor>,
    account: AccountId,
    symbols: HashSet<Symbol>,
}

impl ManagerActor {
    /// 预加载所有交易所的 symbol metas（静态方法）
    async fn preload_all_symbol_metas(
        clients: &HashMap<Exchange, Arc<dyn ExchangeClient>>,
    ) -> Result<HashMap<(Exchange, Symbol), SymbolMeta>, ExchangeError> {
        let mut symbol_metas = HashMap::new();

        for (exchange, client) in clients {
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                symbol_metas.insert((*exchange, meta.symbol.clone()), meta);
            }
            tracing::info!(%exchange, count, "Preloaded symbol metas");
        }

        Ok(symbol_metas)
    }

    /// 提取指定交易所的 symbol metas（静态方法）
    fn get_symbol_metas_for(
        symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
        exchange: Exchange,
    ) -> Arc<HashMap<Symbol, SymbolMeta>> {
        Arc::new(
            symbol_metas
                .iter()
                .filter(|((e, _), _)| *e == exchange)
                .map(|((_, s), m)| (s.clone(), m.clone()))
                .collect(),
        )
    }
}

impl Actor for ManagerActor {
    type Args = ManagerActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 收下 bin 侧装配好的交易所（阶段 1 的 client 已建好；阶段 2 的 actor 装配
        //    延迟到 metas 就绪后并发执行）。manager 只做收集与循环，不知道具体是哪些所。
        let setups = args.exchanges;

        //    模拟盘也需要 client：symbol metas 走公共 REST，与是否有凭证无关
        let clients: HashMap<Exchange, Arc<dyn ExchangeClient>> =
            setups.iter().map(|s| (s.exchange, s.client.clone())).collect();
        // 有凭证 = 有账户可对账；只接公共行情的所没有持仓可言，不必轮询
        let authed_exchanges: HashSet<Exchange> =
            setups.iter().filter(|s| s.authed).map(|s| s.exchange).collect();
        // 能力表：投产校验用（知识在各适配层的 supports_subscription，随 setup 携带）
        let capabilities: HashMap<Exchange, fn(&SubscriptionKind) -> bool> =
            setups.iter().map(|s| (s.exchange, s.supports)).collect();

        // 2. 预加载所有交易所的 symbol metas
        let symbol_metas = Self::preload_all_symbol_metas(&clients).await?;

        // 3. 创建 PubSub Actors：行情 / 账户 / 信号 三条总线。
        //    账户事件（实盘适配层标 Live + 本地柜台标 Paper）走同一条 AccountPubSub，
        //    账户隔离由事件自带的 account 字段保证 —— 不靠总线拓扑，也不靠来源推断。
        let mut pipeline = ChildGroup::default();
        let mut producers = ChildGroup::default();
        let market_pubsub = pipeline.spawn::<MarketPubSub, _>(
            &actor_ref,
            "MarketPubSub",
            MarketPubSub::new(DeliveryStrategy::BestEffort),
        )
        .await;
        let account_pubsub = pipeline.spawn::<AccountPubSub, _>(
            &actor_ref,
            "AccountPubSub",
            AccountPubSub::new(DeliveryStrategy::BestEffort),
        )
        .await;
        let outcome_pubsub = pipeline.spawn::<OutcomePubSub, _>(
            &actor_ref,
            "OutcomePubSub",
            OutcomePubSub::new(DeliveryStrategy::BestEffort),
        )
        .await;

        // 4. 创建 ProcessorActor：订阅两条入向总线，按订阅关系/账户分发给 executor
        let processor = pipeline.spawn::<IncomeProcessorActor, _>(
            &actor_ref,
            "IncomeProcessorActor",
            IncomeProcessorActor::default(),
        )
        .await;
        market_pubsub
            .tell(Subscribe(processor.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        account_pubsub
            .tell(Subscribe(processor.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5. 创建 OutcomeProcessorActor 并订阅 outcome_pubsub
        // 5. OutcomePubSub 的两个消费者**常驻并存**：实盘账户的订单发往交易所，模拟账户的
        //    订单落本地柜台。二者互不知情，各按 AccountOutcome 上的账户取自己的那份 ——
        //    这使得同一 symbol 上模拟与实盘可以并行（模拟先跑，出信号后再拉起实盘）。
        //
        //    OrderGateway 是**唯一**的实盘下单出口：总线路径（OutcomeProcessorActor）与
        //    降级平仓（RemoveStrategies）共享同一实例 —— in_flight 判据、失败反馈、
        //    dry_run 语义对一切下单一致，系统不存在第二个下单出口。
        let order_gateway = Arc::new(OrderGateway::new(
            clients.clone(),
            account_pubsub.clone(),
            // 出向单位折算在此完成（见 ExchangeOrder）；策略与回测全程币本位
            Arc::new(symbol_metas.clone()),
            /* dry_run */ false,
        ));
        let live_processor = pipeline.spawn::<OutcomeProcessorActor, _>(
            &actor_ref,
            "OutcomeProcessorActor",
            order_gateway.clone(),
        )
        .await;
        outcome_pubsub
            .tell(Subscribe(live_processor.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        // 出向处理器还要看**交易所回报**，用来作废迟到的 REST 失败结论
        // （见 OrderGateway::in_flight）。只订 OrderUpdate，不吃全量账户事件。
        account_pubsub
            .tell(SubscribeFilter(live_processor, |e: &AccountEvent| {
                matches!(e.data, AccountData::OrderUpdate(_))
            }))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 本地柜台：订阅信号总线（认领 Paper 账户的订单）与行情总线（撮合 + Clock 发净值），
        // 回报以 Paper 标签发布到 AccountPubSub —— 与实盘适配层同一条总线、同一个类型
        let paper_counter = pipeline.spawn::<PaperCounterActor, _>(
            &actor_ref,
            "PaperCounterActor",
            PaperCounterArgs {
                account_pubsub: account_pubsub.clone(),
                config: args.paper,
                symbol_metas: Arc::new(symbol_metas.clone()),
            },
        )
        .await;
        outcome_pubsub
            .tell(Subscribe(paper_counter.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        market_pubsub
            .tell(Subscribe(paper_counter.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5.5 创建 MetricsActor 并订阅两条入向总线（观测层：只读事件流，不参与决策）
        let metrics = pipeline.spawn::<MetricsActor, _>(
            &actor_ref,
            "MetricsActor",
            MetricsActorArgs {
                interval_ms: DEFAULT_REPORT_INTERVAL_MS,
                // PUSHGATEWAY_URL 环境变量启用，未设置 = 只输出日志
                push: crate::observability::MetricsPushConfig::from_env(),
            },
        )
        .await;
        market_pubsub
            .tell(Subscribe(metrics.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        // 账户事件（实盘 + 全部模拟账户）一次订阅；MetricsActor 按事件自带的账户分账，
        // 绝不与实盘视图混淆
        account_pubsub
            .tell(Subscribe(metrics.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5.6 持仓对账（通道 B）：REST 读数 vs 本地「基线 + Fill」。
        //     与 MetricsActor 分开是刻意的——观测层故障不该拖垮交易，而对账确认漂移必须
        //     致命退出（本地持仓不可信时，策略的三道风控闸门全部失效）。
        //
        //     读数由它**自行轮询**：读数只有这一个消费者，不上总线；每个有凭证的所一个
        //     client（无凭证的所没有持仓可言）。`fetch_positions` 在 ExchangeClient trait
        //     上，交易所模块无需改动。
        let authed_clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = clients
            .iter()
            .filter(|(e, _)| authed_exchanges.contains(e))
            .map(|(e, c)| (*e, c.clone()))
            .collect();
        let position_reconciler = pipeline.spawn::<PositionReconcileActor, _>(
            &actor_ref,
            "PositionReconcileActor",
            PositionReconcileArgs {
                clients: authed_clients,
                symbol_metas: Arc::new(symbol_metas.clone()),
                interval_ms: DEFAULT_POSITION_POLL_INTERVAL_MS,
                max_consecutive_mismatches: DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
            },
        )
        .await;
        // 镜像的增量输入只有实盘 Fill：按类型+账户过滤订阅（模拟成交绝不进实盘镜像）
        account_pubsub
            .tell(SubscribeFilter(position_reconciler.clone(), |e: &AccountEvent| {
                e.account == AccountId::Live && matches!(e.data, AccountData::Fill(_))
            }))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 6. 创建 ClockActor（发布 Clock 事件到行情总线）
        producers.spawn::<ClockActor, _>(
            &actor_ref,
            "ClockActor",
            ClockActorArgs {
                interval_ms: 1000,
                market_pubsub: market_pubsub.clone(),
            },
        )
        .await;

        // 7. 并发装配各所的 WS actor 集：spawn 瞬间返回，各 setup 的 future 里等
        //    on_start 完成（耗时的 IO 部分），join_all 让四家并发。任一失败 -> 向上
        //    传播 ExchangeError（启动期受控退出，不重试/不重连）。
        let spawn_futures: Vec<_> = setups
            .into_iter()
            .map(|setup| {
                let ctx = SpawnCtx {
                    manager: actor_ref.clone(),
                    market_pubsub: market_pubsub.clone(),
                    account_pubsub: account_pubsub.clone(),
                    symbol_metas: Self::get_symbol_metas_for(&symbol_metas, setup.exchange),
                };
                let exchange = setup.exchange;
                async move { (setup.spawn_actor)(ctx).await.map(|(ops, stop)| (exchange, ops, stop)) }
            })
            .collect();
        let mut exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>> = HashMap::new();
        for result in futures_util::future::join_all(spawn_futures).await {
            let (exchange, ops, stop) = result?;
            tracing::info!(%exchange, "ExchangeActor ready");
            exchange_actors.insert(exchange, ops);
            producers.push_handle(stop);
        }

        tracing::info!("ManagerActor started with all child actors linked");

        Ok(Self {
            symbol_metas,
            clients,
            capabilities,
            market_pubsub,
            account_pubsub,
            outcome_pubsub,
            income_processor: processor,
            metrics,
            position_reconciler,
            exchange_actors,
            order_gateway,
            executors: Vec::new(),
            producers,
            pipeline,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // 先停生产端、再停管线。顺序反了的话，生产者 on_stop 里补发的最后一批事件
        // （如 IbkrPublicWsActor 等 commission 的成交）会发进一个已经死掉的总线 ——
        // 那正是本次停机链要防的失效。
        //
        // 递归自动成立：每个交易所 actor 的 on_stop 又在等它自己那批 WS / 轮询子 actor。
        self.producers.shutdown().await;
        self.pipeline.shutdown().await;
        tracing::info!(reason = ?reason, "ManagerActor stopped");
        Ok(())
    }

    /// 监督树上的 actor 终止时的处置：**主动退出放行，出错级联退出**。
    ///
    /// 判据就是 kameo 给的 [`ActorStopReason`]：`Normal` 意味着有人调用了
    /// `stop_gracefully()`（降级 [`RemoveStrategies`]、投产失败回滚、外部观察者收工），
    /// 引擎继续运行；`Killed` / `Panicked` / `LinkDied` 都是事故，一律受控停机，交由外部
    /// 编排重启 —— 本项目不重连、不降级运行。
    ///
    /// # 这条判据成立的前提：没有 actor 会悄悄以 `Normal` 死掉
    ///
    /// `Normal` 在 kameo 里有两个来源：`stop_gracefully()`，以及**邮箱所有 sender 被 drop**
    /// （`kind.rs` 里 `next()` 返回 `None` 那条）。后者若可达，"时钟停了""对账停了"就会被
    /// 当成正常退出而静默忽略 —— 判据也就废了。
    ///
    /// 引擎靠两条保证把后者堵死，**新增 actor 时必须继续满足其一**：
    ///
    /// - **流驱动的 actor 在流结束时 `kill()` 自己**（`Killed`，响亮）。`attach_stream` 会在
    ///   任务丢引用**之前**先投递 `StreamMessage::Finished`，所以这一步一定来得及。WS 类由
    ///   [`crate::dispatch_ws_stream_message`] 统一保证，各 polling actor 在自己的 `Finished`
    ///   分支里做同样的事。
    /// - **manager 持有强引用**（`exchange_actors` / `metrics` / `income_processor` /
    ///   `position_reconciler` / `executors`），压根饿不死。
    ///
    /// 外部注册的观察者由 [`crate::engine::spawn_supervised`] 挂进来，调用方持有返回的
    /// ActorRef，同样不会饿死。
    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        if reason.is_normal() {
            tracing::info!(actor_id = ?id, "受监督 actor 已主动退出，引擎继续运行");
            return Ok(ControlFlow::Continue(()));
        }
        tracing::error!(actor_id = ?id, reason = ?reason, "受监督 actor 出错终止，整机退出");
        Ok(ControlFlow::Break(ActorStopReason::LinkDied {
            id,
            reason: Box::new(reason),
        }))
    }
}

// ============================================================================
// Messages
// ============================================================================

/// 添加策略
/// 策略实例 + 它绑定的账户。
///
/// 同一份策略逻辑可以用不同账户注册两次：一份跑模拟账户、一份跑实盘账户，同时运行。
pub struct StrategySpec {
    pub strategy: Box<dyn Strategy>,
    pub account: AccountId,
}

impl StrategySpec {
    /// 绑定到实盘账户
    pub fn live(strategy: Box<dyn Strategy>) -> Self {
        Self {
            strategy,
            account: AccountId::Live,
        }
    }

    /// 绑定到指定标签的模拟账户（本项目按 symbol 分账）
    pub fn paper(strategy: Box<dyn Strategy>, label: impl Into<String>) -> Self {
        Self {
            strategy,
            account: AccountId::Paper(label.into()),
        }
    }
}

pub struct AddStrategy(pub StrategySpec);

impl Message<AddStrategy> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: AddStrategy,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 委托给批量添加
        self.do_add_strategies(vec![msg.0], ctx.actor_ref().clone())
            .await
    }
}

/// 批量添加策略
pub struct AddStrategies(pub Vec<StrategySpec>);

impl Message<AddStrategies> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: AddStrategies,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.do_add_strategies(msg.0, ctx.actor_ref().clone()).await
    }
}



/// 把一个外部受监督 actor 登记进 manager 的停机链，供 [`crate::engine::spawn_supervised`]
/// 使用，不要直接发。
///
/// 缺了这一步，观察者虽然 link 在监督树上（出错会级联），停机时却没人等它收尾 ——
/// 它会一直活到 runtime drop 才被硬砍。
pub struct RegisterSupervisedChild(pub ChildStop);

impl Message<RegisterSupervisedChild> for ManagerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RegisterSupervisedChild,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.pipeline.push_handle(msg.0);
    }
}

/// 订阅 Income 事件（行情/账户事件流，供外部观察者导出指标或通知）
///
/// # 只收受监督的 actor
///
/// 入参是 [`Supervised<A>`]，只能由 [`crate::engine::spawn_supervised`] 产出 —— 于是
/// "订一个没人看着的 actor"在类型上就写不出来。这道门是必要的：订阅本身不建立任何
/// 监督关系，订阅者若已经死了（或订阅之后死掉），投递遇 `ActorNotRunning` 会把它
/// **静默摘除**，事件从此不再送达，而调用方一无所知。
///
/// # 退订方式
///
/// 底层 PubSub（kameo_actors）没有显式退订原语：**订阅者 actor 停机即自动被摘除**
/// （投递遇 ActorNotRunning 时移除该订阅者）。受监督的订阅者要收工，对自己的
/// [`Supervised::actor_ref`] 调 `stop_gracefully()` 即可 —— 那产生 `Normal`，
/// [`ManagerActor::on_link_died`] 据此放行，引擎照常运行，不需要另行声明什么。
pub struct SubscribeMarket<A: Actor> {
    actor: Supervised<A>,
    filter: fn(&MarketEvent) -> bool,
}

impl<A: Actor> SubscribeMarket<A> {
    /// 订阅全部行情事件
    pub fn all(actor: Supervised<A>) -> Self {
        Self { actor, filter: |_| true }
    }

    /// 只订阅 `filter` 返回 true 的事件。
    ///
    /// 过滤在 PubSub 自己的任务里执行：不通过的事件**连 clone 都省掉**，订阅者也不会
    /// 被唤醒。观测类订阅者往往只关心少数几种事件，全收等于每个行情 tick 都白唤醒一次。
    ///
    /// `filter` 是 `fn` 指针不是闭包，**不能捕获**任何东西 —— 判据必须只看事件本身，
    /// 这样"我订了什么"是一段可以直接读懂的静态声明，而不是运行期才知道的行为。
    pub fn only(actor: Supervised<A>, filter: fn(&MarketEvent) -> bool) -> Self {
        Self { actor, filter }
    }
}

impl<A> Message<SubscribeMarket<A>> for ManagerActor
where
    A: Actor + Message<MarketEvent>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeMarket<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self
            .market_pubsub
            .tell(SubscribeFilter(msg.actor.into_inner(), msg.filter))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to subscribe to MarketPubSub");
        }
    }
}

/// 订阅账户私有事件（实盘 + 全部模拟账户在同一条总线上，按事件自带的 `account`
/// 字段区分）。退订方式见 [`SubscribeMarket`]（订阅者停机即自动摘除）。
pub struct SubscribeAccount<A: Actor> {
    actor: Supervised<A>,
    filter: fn(&AccountEvent) -> bool,
}

impl<A: Actor> SubscribeAccount<A> {
    /// 订阅全部账户事件（实盘与全部模拟账户）
    pub fn all(actor: Supervised<A>) -> Self {
        Self { actor, filter: |_| true }
    }

    /// 只订阅 `filter` 返回 true 的事件（可按 `account` 与事件类型过滤）。
    /// `filter` 是 `fn` 指针不是闭包，**不能捕获**任何东西，同 [`SubscribeMarket::only`]。
    pub fn only(actor: Supervised<A>, filter: fn(&AccountEvent) -> bool) -> Self {
        Self { actor, filter }
    }
}

impl<A> Message<SubscribeAccount<A>> for ManagerActor
where
    A: Actor + Message<AccountEvent>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeAccount<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self
            .account_pubsub
            .tell(SubscribeFilter(msg.actor.into_inner(), msg.filter))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to subscribe to AccountPubSub");
        }
    }
}

/// 外部向策略注入自定义事件（[`crate::messaging::CustomEvent`] 的**入向**入口）。
///
/// 事件进入行情总线后按 scope 路由：带 `(exchange, symbol)` 的只投递给订阅了
/// 该 symbol 的策略，无 scope 的广播 —— 与行情同一套分发，无第二条通道。策略在
/// `on_event` 里 match `MarketData::Custom` 并按类型 `get::<T>()` 消费。
///
/// 自定义事件没有账户归属（如同行情）：实盘与模拟账户的策略都会收到。总线上的
/// 非 executor 订阅者（观测镜像等）同样可见。时间戳取注入时刻。
pub struct PublishCustomEvent(pub crate::messaging::CustomEvent);

impl Message<PublishCustomEvent> for ManagerActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: PublishCustomEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let ts = now_ms();
        // 注入留痕：事件"发了没人收"（scope 拼错、订阅缺失、类型不符）时，这是唯一的定位起点
        tracing::debug!(name = msg.0.name, scope = ?msg.0.scope(), "注入自定义事件");
        let event = MarketEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: MarketData::Custom(msg.0),
        };
        if let Err(e) = self
            .market_pubsub
            .tell(kameo_actors::pubsub::Publish(event))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish custom event to MarketPubSub");
        }
    }
}

/// 订阅 Outcome 事件
/// 退订方式见 [`SubscribeIncome`]（订阅者停机即自动摘除）。
pub struct SubscribeOutcome<A: Actor> {
    actor: Supervised<A>,
    filter: fn(&AccountOutcome) -> bool,
}

impl<A: Actor> SubscribeOutcome<A> {
    /// 订阅全部事件
    pub fn all(actor: Supervised<A>) -> Self {
        Self { actor, filter: |_| true }
    }

    /// 只订阅 `filter` 返回 true 的事件。
    ///
    /// 过滤在 PubSub 自己的任务里执行：不通过的事件**连 clone 都省掉**，订阅者也不会
    /// 被唤醒。
    ///
    /// `filter` 是 `fn` 指针不是闭包，**不能捕获**任何东西 —— 判据必须只看事件本身，
    /// 这样"我订了什么"是一段可以直接读懂的静态声明，而不是运行期才知道的行为。
    pub fn only(actor: Supervised<A>, filter: fn(&AccountOutcome) -> bool) -> Self {
        Self { actor, filter }
    }
}

impl<A> Message<SubscribeOutcome<A>> for ManagerActor
where
    A: Actor + Message<AccountOutcome>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeOutcome<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self
            .outcome_pubsub
            .tell(SubscribeFilter(msg.actor.into_inner(), msg.filter))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish to OutcomePubSub");
        }
    }
}

/// 获取所有交易所的 SymbolMeta
pub struct GetAllSymbolMetas;

impl Message<GetAllSymbolMetas> for ManagerActor {
    type Reply = HashMap<Exchange, Vec<SymbolMeta>>;

    async fn handle(
        &mut self,
        _msg: GetAllSymbolMetas,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut result: HashMap<Exchange, Vec<SymbolMeta>> = HashMap::new();
        for ((exchange, _), meta) in &self.symbol_metas {
            result.entry(*exchange).or_default().push(meta.clone());
        }
        result
    }
}


#[cfg(test)]
mod stop_semantics_tests {
    use super::*;
    use kameo::actor::Spawn;
    use kameo::error::Infallible;
    use kameo::mailbox;
    use std::sync::Mutex;
    use std::time::Duration;

    /// 复刻 `ManagerActor::on_link_died` 的路由规则，验证其**判据本身**。
    ///
    /// 用一对最小父子而非真 `ManagerActor`：后者的 `on_start` 要联网建 client、拉 symbol
    /// metas，无法在单测里起来。
    struct Child;

    impl Actor for Child {
        type Args = ();
        type Error = Infallible;
        async fn on_start(_: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
    }

    struct Parent {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Actor for Parent {
        type Args = Arc<Mutex<Vec<String>>>;
        type Error = Infallible;
        async fn on_start(args: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self { seen: args })
        }
        async fn on_link_died(
            &mut self,
            _: WeakActorRef<Self>,
            id: ActorId,
            reason: ActorStopReason,
        ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
            self.seen.lock().unwrap().push(format!("{reason:?}"));
            // 与 ManagerActor 一致：按 stop reason 判 —— 主动退出放行，出错级联
            if reason.is_normal() {
                return Ok(ControlFlow::Continue(()));
            }
            Ok(ControlFlow::Break(ActorStopReason::LinkDied {
                id,
                reason: Box::new(reason),
            }))
        }
    }

    /// `graceful` 决定子 actor 是主动停（Normal）还是被 kill（Killed）
    async fn run(graceful: bool) -> (bool, Vec<String>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let parent = Parent::spawn_with_mailbox(seen.clone(), mailbox::unbounded());
        let child = Child::spawn_link_with_mailbox(&parent, (), mailbox::unbounded()).await;

        if graceful {
            child.stop_gracefully().await.expect("发送 Stop");
        } else {
            child.kill();
        }
        child.wait_for_shutdown().await;
        // 给 parent 的 on_link_died 一点时间跑完
        tokio::time::sleep(Duration::from_millis(50)).await;

        let reasons = seen.lock().unwrap().clone();
        (parent.is_alive(), reasons)
    }

    /// **主动退出**（stop_gracefully → Normal）→ 父 actor 必须活着。
    ///
    /// 对应降级（RemoveStrategies）、投产失败回滚，以及外部观察者收工：
    /// 撤下一个实例不该把整个引擎带走。
    #[tokio::test]
    async fn graceful_stop_lets_the_parent_survive() {
        let (parent_alive, reasons) = run(true).await;
        assert_eq!(reasons, vec!["Normal".to_string()]);
        assert!(parent_alive, "主动撤下子 actor 却把父 actor 带走了");
    }

    /// **出错死亡**（kill → Killed）→ 级联退出。
    ///
    /// 守住"WS 解析失败 / 流意外结束自 kill 即整机受控停机"这条既有约定 ——
    /// 各流驱动 actor 的 `StreamMessage::Finished` 分支正是靠 kill 把静默的
    /// `Normal` 变成响亮的 `Killed`，判据才敢只看 reason（见 `on_link_died` 文档）。
    #[tokio::test]
    async fn killed_child_takes_the_parent_down() {
        let (parent_alive, reasons) = run(false).await;
        assert_eq!(reasons, vec!["Killed".to_string()]);
        assert!(!parent_alive, "子 actor 异常死亡却没有级联退出");
    }
}


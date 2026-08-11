//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 创建 PubSub Actors (IncomePubSub, OutcomePubSub)
//! - 使用 spawn_link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use crate::actor_lifecycle::{ChildGroup, ChildStop};
use crate::engine::bootstrap::Supervised;
use super::{
    AccountIncome, AccountOutcome, ClockActor, ClockActorArgs, ExecutorActor, ExecutorArgs, IncomePubSub,
    PaperCounterActor, PaperCounterArgs, PaperPubSub,
    GetPositions, IncomeProcessorActor, MetricsActor, MetricsActorArgs, OutcomePubSub,
    OutcomeProcessorActor,
    PositionPollingActor, PositionPollingActorArgs, PositionReconcileActor, PositionReconcileArgs,
    OrderGateway, RegisterExecutor, RegisterSymbols, UnregisterExecutor,
    DEFAULT_MAX_CONSECUTIVE_MISMATCHES, DEFAULT_POSITION_POLL_INTERVAL_MS,
    DEFAULT_REPORT_INTERVAL_MS,
};
use crate::domain::{
    now_ms, AccountId, Exchange, ExchangeError, Order, OrderType, OrderUpdate, Side, Symbol,
    SymbolMeta,
};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::exchange::binance::{
    BinanceActor, BinanceActorArgs, BinanceClient, BinanceCredentials, REST_BASE_URL,
};
use crate::exchange::hyperliquid::{
    HyperliquidActor, HyperliquidActorArgs, HyperliquidClient, HyperliquidCredentials,
};
use crate::exchange::ibkr::{IbkrActor, IbkrActorArgs, IbkrClient, IbkrCredentials, IbkrSnapshotConfig};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxClient, OkxCredentials};
use crate::exchange::{
    ExchangeAccess, ExchangeActorOps, ExchangeClient, SubscriptionKind,
};
use crate::strategy::Strategy;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::{Subscribe, SubscribeFilter};
use kameo_actors::DeliveryStrategy;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

/// 等待一个 executor 有序停下的上限。
///
/// `ExecutorActor` 的 handler 是纯策略步进 + 往 unbounded 邮箱投递，都是亚毫秒级工作，
/// 正常远不到这个量级。留 5 秒是为了不因偶发调度抖动就走异常分支。
const EXECUTOR_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// ManagerActor 初始化参数
pub struct ManagerActorArgs {
    /// Binance 接入配置（缺省即该所不参与）
    pub binance: Option<ExchangeAccess<BinanceCredentials>>,
    /// OKX 接入配置
    pub okx: Option<ExchangeAccess<OkxCredentials>>,
    /// Hyperliquid 接入配置
    pub hyperliquid: Option<ExchangeAccess<HyperliquidCredentials>>,
    /// IBKR 凭证（可选）
    pub ibkr_credentials: Option<IbkrCredentials>,
    /// IBKR 借券费/汇率 snapshot 轮询配置（可选）：Some 时 IbkrActor 会 spawn_link 一个
    /// 轮询子 actor，定时发 BorrowFee/ExchangeRate；数据源失效即致命退出（无兜底常量）。
    pub ibkr_snapshot: Option<IbkrSnapshotConfig>,
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

    // === PubSub Actors ===
    /// Income PubSub (行情/账户事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Outcome PubSub (策略信号)
    outcome_pubsub: ActorRef<OutcomePubSub>,

    // === 子 Actors ===
    /// ProcessorActor (订阅 income_pubsub)
    income_processor: ActorRef<IncomeProcessorActor>,
    /// MetricsActor (订阅 income_pubsub，输出账户/持仓/订单/历史指标)
    metrics: ActorRef<MetricsActor>,
    /// 持仓对账 Actor (订阅 income_pubsub；漂移确认后致命退出)
    position_reconciler: ActorRef<PositionReconcileActor>,
    /// ExchangeActors (启动时创建，类型擦除)
    exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>>,

    /// 模拟账户私有事件总线（供 Supervisor 等观察者订阅）
    paper_pubsub: ActorRef<PaperPubSub>,

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

    /// 引擎生命周期内已向总线发布过持仓基线的 (所, symbol)。
    ///
    /// 总线上的基线只喂**引擎生命周期**的镜像（对账/观测），每个 pair 只许一次——镜像的
    /// 「基线 + Fill」账本在策略实例撤下期间也一直在跟 Fill，重发会被判成"重复基线"违约。
    /// 晋升/降级再晋升时，新 executor 的基线走点对点投递（见 [`Self::activate_executors`]），
    /// 不依赖总线重发。
    baselined_positions: HashSet<(Exchange, Symbol)>,
}

/// 投产期订阅可行性校验（纯函数，供单测）。
///
/// 逐条检查策略声明的 (exchange, kind)：交易所未配置（不在 `available` 里）、适配层
/// 不支持该 kind（见 [`crate::exchange::supports_subscription`] 能力表）、或该 symbol
/// **没有 SymbolMeta**，都返回 Err。一次性汇总全部问题，不在第一条就停 —— 让策略作者
/// 一次看全。
///
/// # 为什么 SymbolMeta 缺失必须在这里拦
///
/// 各所的元数据加载都会剔除字段异常的合约（OKX 的 `filter_map`、Binance 的 step 解析）。
/// 被剔掉的 symbol 若正好是策略要交易的，故障会以三种毫不相干的面貌分散出现：下单被
/// "No SymbolMeta" 拒、私有回报因无法折算张数被丢弃（**持仓从此落后于交易所**）、行情
/// 订阅被忽略。三处都只是症状，根因是"配置要交易的 symbol 没有元数据"，而这一点在投产
/// 期一次就能查清 —— 此时尚无任何副作用，Err 即"什么都没发生"。
fn validate_subscriptions(
    subscriptions: &HashSet<(Exchange, SubscriptionKind)>,
    available: &HashSet<Exchange>,
    symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    for (exchange, kind) in subscriptions {
        if !available.contains(exchange) {
            problems.push(format!("{exchange} 未配置（{kind:?} 无从订阅）"));
        } else if !crate::exchange::supports_subscription(*exchange, kind) {
            problems.push(format!("{exchange} 的适配层不支持 {kind:?}"));
        } else if !symbol_metas.contains_key(&(*exchange, kind.symbol().clone())) {
            problems.push(format!(
                "{exchange}/{} 没有 SymbolMeta（元数据加载时被剔除或该合约不存在）\
                 —— 下单会被拒、私有回报会因无法折算张数被丢弃",
                kind.symbol()
            ));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    problems.sort(); // 确定的报错顺序
    Err(format!("策略订阅不可行，拒绝投产: {}", problems.join("; ")))
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

    /// 有序撤下一个 executor：发出 Stop、并**等它真的停下**。
    ///
    /// # 为什么不能用 `kill()`
    ///
    /// `kill()` 产生 [`ActorStopReason::Killed`]，与"意外崩溃"在 reason 上无法区分 —— 而
    /// `ManagerActor::on_link_died` 会把异常终止判为"子 actor 挂了"并整机退出。**有意撤下
    /// 一个策略实例不该打死引擎**（降级正是这么做的）。`stop_gracefully()` 产生 `Normal`，
    /// `on_link_died` 据此放行，两种情形就此可分。
    ///
    /// # 为什么必须等
    ///
    /// `stop_gracefully()` 只是投递 Stop 信号；实例会先排空邮箱才停，期间**仍可能产出订单**。
    /// 不等就往下走（撤单 / 平仓），会出现"我们撤完单，它才把新单发出去"的竞态 —— 那张新单
    /// 又成了无人看管的遗留挂单。
    ///
    /// 返回 `Err(原因)` 供调用方计入"未完成"；超时不改用 `kill()`（那会打死引擎），而是如实
    /// 报出 —— 此时该实例已从 IncomeProcessor 注销，收不到新事件，危害有界。
    async fn stop_executor(&mut self, executor: &ActorRef<ExecutorActor>) -> Result<(), String> {
        // 必须是 stop_gracefully 而非 kill：前者产生 `Normal`，`on_link_died` 据此放行；
        // kill 产生 `Killed`，会被判成事故、把整个引擎带走。
        if let Err(e) = executor.stop_gracefully().await {
            return Err(format!("发送 Stop 信号失败: {e}"));
        }
        let stopped = tokio::time::timeout(EXECUTOR_STOP_TIMEOUT, executor.wait_for_shutdown())
            .await
            .map_err(|_| format!("等待实例停止超时（{}s）", EXECUTOR_STOP_TIMEOUT.as_secs()));

        // **确认停下来了**才摘出停机链：撤下的实例不该再攒着强引用（晋升/降级反复
        // 几轮就是泄漏）。但没停下来的必须留着 —— 摘掉等于进程停机时没人再等它，
        // 那它的收尾就会被 runtime drop 砍断，而这正是这套停机链要防的事。
        // 留着的代价有界：它已从 IncomeProcessor 注销、收不到新事件，最坏是让停机
        // 多等一会儿，并由根部的总 deadline 如实报出来。
        if stopped.is_ok() {
            self.producers.remove(executor);
        }
        stopped
    }

    /// 列出**本引擎**在该 (所, symbol) 上的挂单。
    ///
    /// 同一个交易所账户上可能还有人工下单或其他程序下的单，靠 client_order_id 的前缀识别
    /// （见 [`Exchange::owns_cli_order_id`]）。认不出归属的一律**保留** —— 撤错别人的单
    /// 不可接受，而漏撤会由调用方的复查兜住。
    async fn own_pending_orders(
        client: &Arc<dyn ExchangeClient>,
        exchange: Exchange,
        symbol: &Symbol,
    ) -> Result<Vec<OrderUpdate>, ExchangeError> {
        let all = client.fetch_pending_orders(symbol).await?;
        Ok(all
            .into_iter()
            .filter(|update| match update.client_order_id.as_deref() {
                Some(id) if exchange.owns_cli_order_id(id) => true,
                Some(id) => {
                    tracing::info!(
                        %exchange, %symbol, client_order_id = id,
                        "挂单不属于本引擎，保留不动"
                    );
                    false
                }
                None => {
                    tracing::info!(
                        %exchange, %symbol, order_id = %update.order_id,
                        "挂单无 client_order_id，无法确认归属，保留不动"
                    );
                    false
                }
            })
            .collect())
    }

    /// 撤掉本引擎在该 (所, symbol) 上遗留的挂单，并**复查确认撤净**。
    ///
    /// 单笔撤单失败不直接判死：它可能只是"撤之前那张单刚好成交或已被撤"这类良性竞态。
    /// 是否真的撤净由**复查**裁定 —— 比按各所错误码猜测"是不是 order not found"可靠得多，
    /// 也不需要为此在错误类型上增加变体。
    ///
    /// 复查后仍有遗留 → 返回 `Err` 拒绝启动。留着一张无人看管的挂单开跑，策略会在它旁边
    /// 重复挂单；而启动期失败是**最便宜的失败时机**（还没开始交易）。
    async fn cancel_leftover_orders(
        client: &Arc<dyn ExchangeClient>,
        exchange: Exchange,
        symbol: &Symbol,
    ) -> Result<(), ExchangeError> {
        let ours = Self::own_pending_orders(client, exchange, symbol).await?;
        if ours.is_empty() {
            return Ok(());
        }

        tracing::warn!(
            %exchange, %symbol, count = ours.len(),
            "[STARTUP] 发现本引擎遗留的挂单，全部撤掉（不接管）"
        );
        for order in &ours {
            if let Err(e) = client.cancel_order(symbol, &order.order_id).await {
                tracing::warn!(
                    %exchange, %symbol, order_id = %order.order_id, error = %e,
                    "撤单请求失败，由复查裁定是否真的还在"
                );
            }
        }

        let remaining = Self::own_pending_orders(client, exchange, symbol).await?;
        if !remaining.is_empty() {
            return Err(ExchangeError::Other(format!(
                "{exchange}/{symbol}: 撤单后复查仍有 {} 张本引擎遗留挂单，拒绝启动\
                 （留着无人看管的挂单开跑，策略会在旁边重复挂单）；order_id: {:?}",
                remaining.len(),
                remaining.iter().map(|o| &o.order_id).collect::<Vec<_>>()
            )));
        }
        Ok(())
    }

    /// 批量添加策略的内部实现
    async fn do_add_strategies(
        &mut self,
        strategies: Vec<StrategySpec>,
        actor_ref: ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        if strategies.is_empty() {
            return Ok(());
        }

        let outcome_pubsub = self.outcome_pubsub.clone();

        // 1. 收集所有策略的订阅，并按交易所分组
        let mut all_subscriptions: HashSet<(Exchange, SubscriptionKind)> = HashSet::new();
        let mut strategy_subscriptions: Vec<HashSet<(Exchange, SubscriptionKind)>> = Vec::new();

        for spec in &strategies {
            let public_streams = spec.strategy.public_streams();
            let subscriptions: HashSet<(Exchange, SubscriptionKind)> = public_streams
                .iter()
                .flat_map(|(exchange, kinds)| {
                    kinds.iter().map(move |kind| (*exchange, kind.clone()))
                })
                .collect();
            all_subscriptions.extend(subscriptions.clone());
            strategy_subscriptions.push(subscriptions);
        }

        // 1.5 投产期校验订阅可行性：交易所未配置、或适配层不支持该 kind，都在此刻
        //     拒绝投产 —— 此前 subscribe 对这两种情况静默返回 Ok(())，策略上线后只能
        //     靠"一直没数据"事后发现（最难查的一类故障）。此时尚无任何副作用，Err 即
        //     "什么都没发生"。
        let available: HashSet<Exchange> = self.exchange_actors.keys().copied().collect();
        validate_subscriptions(&all_subscriptions, &available, &self.symbol_metas)
            .map_err(ExchangeError::Other)?;

        // 2. 按交易所分组订阅
        let mut exchange_subscriptions: HashMap<Exchange, Vec<SubscriptionKind>> = HashMap::new();
        for (exchange, kind) in &all_subscriptions {
            exchange_subscriptions
                .entry(*exchange)
                .or_default()
                .push(kind.clone());
        }

        // 3. 本次涉及的 (exchange, symbol) 对（下面多步共用）
        let exchange_symbols: HashSet<(Exchange, Symbol)> = all_subscriptions
            .iter()
            .map(|(exchange, kind)| (*exchange, kind.symbol().clone()))
            .collect();

        // 4. 撤掉本引擎在这些 symbol 上遗留的挂单（**不接管**）。
        //
        //    为什么撤而不是接管：策略的内存态重启后已经丢了 —— 比如 gamma_scalp 记着"哪张是
        //    当前对冲单"、以及自己的 cancelling 集合。接管一张挂单还得**伪造 tif**（交易所的
        //    订单更新不带这个信息），拿半真半假的字段去做 side_changed / qty_drift 判断。
        //    本项目也没有队列位置模型，撤掉重挂不损失任何东西。
        //
        //    不撤的后果是实打实的：本地 pending 为空 → gamma_scalp 的 maintain() 走 None
        //    分支 → 在旧单旁边**再挂一张**对冲单，而它的契约是"维护单张对冲单"。
        //
        //    **刻意放在创建 executor 之前**：这一步只要 client + symbol，不依赖 executor。
        //    提前的好处是它的失败**根本不需要回滚** —— 此时还没有任何实例被建出来，
        //    直接返回 Err 就等于"什么都没发生"。
        for (exchange, symbol) in &exchange_symbols {
            let Some(client) = self.clients.get(exchange) else {
                continue;
            };
            Self::cancel_leftover_orders(client, *exchange, symbol).await?;
        }

        // 5. 批量创建 ExecutorActors（此刻它们还没被注册，收不到任何事件）
        let mut executor_refs = Vec::new();
        for spec in strategies {
            let account = spec.account.clone();
            let executor_ref = self
                .producers
                .spawn::<ExecutorActor, _>(
                    &actor_ref,
                    "ExecutorActor",
                    ExecutorArgs {
                        strategy: spec.strategy,
                        account: spec.account,
                        symbol_metas: Arc::new(self.symbol_metas.clone()),
                        outcome_pubsub: outcome_pubsub.clone(),
                    },
                )
                .await;
            executor_refs.push((executor_ref, account));
        }

        // 6. 注册并投产。**从这里开始产生副作用**，故任一步失败都要回滚已注册的实例 ——
        //    两个调用方（bin 的 main、SupervisorActor::promote）都把 Err 理解成"什么都没
        //    发生"：main 会退出进程（残留无所谓），而 Supervisor 会保持 live=None、以为没
        //    晋升成功。若此时留着一个绑定 AccountId::Live 的实例在跑，它会收私有事件、会真
        //    下单，而 Supervisor 永远不会去 demote 它 —— 无人看管的实盘实例。
        if let Err(e) = self
            .activate_executors(
                &executor_refs,
                &strategy_subscriptions,
                &exchange_symbols,
                exchange_subscriptions,
            )
            .await
        {
            self.rollback_executors(&executor_refs).await;
            return Err(e);
        }

        tracing::info!(
            count = executor_refs.len(),
            "Strategies batch added, ExecutorActors created"
        );

        Ok(())
    }

    /// 把已创建的 executor 注册进事件流并投产：注册观测/对账范围 → 拉取并发布持仓基线
    /// → 点对点投递基线 → 注册订阅 → 放行行情。
    ///
    /// # 时序不变量：基线必须先于任何流事件抵达消费者
    ///
    /// - **executor**：基线在注册（6.5）**之前**点对点投进邮箱（6.4），FIFO 保证它先于
    ///   任何流事件被处理 —— "Fill 早于基线、基线被判重复而丢掉存量"的竞态结构上不可能。
    /// - **镜像**（对账/观测）：基线经总线送达（6.3），紧跟范围注册（6.1）发布以压窄窗口；
    ///   残余窗口由对账层的 Fill 缓冲重放兜住。
    ///
    /// 拆成独立方法只为一件事：让 [`Self::do_add_strategies`] 能在这里失败时统一回滚，
    /// 而不必在每个 `?` 旁边重复一遍回滚代码。
    async fn activate_executors(
        &mut self,
        executor_refs: &[(ActorRef<ExecutorActor>, AccountId)],
        strategy_subscriptions: &[HashSet<(Exchange, SubscriptionKind)>],
        exchange_symbols: &HashSet<(Exchange, Symbol)>,
        exchange_subscriptions: HashMap<Exchange, Vec<SubscriptionKind>>,
    ) -> Result<(), ExchangeError> {
        // 6.1 告知观测层与对账层要跟踪哪些 symbol。**必须在发布基线之前**——否则观测层会
        //     因 symbol 未注册而丢掉基线、进而丢掉盈亏基线；对账层的镜像里没有该 symbol 的
        //     状态，基线也落不进去。由 manager 自己从策略订阅推导并注册，调用方无需（也无从）
        //     关心这个时序。用 `ask` 而非 `tell`：必须确认注册完成后才继续推基线。
        {
            let symbols: Vec<Symbol> = exchange_symbols
                .iter()
                .map(|(_, symbol)| symbol.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            // 注册失败必须向上传播，与 6.1 同理：对账层注册不上，该 symbol 的基线会被
            // 镜像丢弃（is_tracked 为假），这条腿的对账通道**静默失效** —— 恰是本函数
            // 通篇要防的"错了没人知道"。观测层同理（盈亏基线丢失，会话口径失真）。
            self.metrics
                .ask(RegisterSymbols(symbols.clone()))
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 MetricsActor 注册 symbol 失败（盈亏基线将丢失）: {e}"
                    ))
                })?;
            self.position_reconciler
                .ask(RegisterSymbols(symbols))
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 PositionReconcileActor 注册 symbol 失败（对账通道将静默失效）: {e}"
                    ))
                })?;
        }

        // 6.2 REST 查询各交易所初始持仓，构造每个 (exchange, symbol) 的基线事件：交易所
        //     返回的用真实数据，未返回的构造 size=0 显式清零（保证消费方一定收到初始值）。
        //     拉取失败即**致命**：持仓基线只有这一次机会，之后全程靠 Fill 累加，
        //     没有第二个来源能纠正它。跳过的后果是策略从"仓位 0"起算——若账户实际有
        //     持仓，三道风控闸门（单边杠杆/账户杠杆/仓位上限）会全部基于错误基线放行。
        //     宁可拒绝启动。
        let mut baselines: HashMap<(Exchange, Symbol), IncomeEvent> = HashMap::new();
        {
            let exchanges: HashSet<Exchange> = exchange_symbols.iter().map(|(e, _)| *e).collect();
            for exchange in exchanges {
                let Some(client) = self.clients.get(&exchange) else {
                    continue;
                };
                // 快照**请求**时刻，作为基线事件的 exchange_ts：在此之前已由私有流送达的
                // Fill，其成交必然已包含在快照里 —— 对账镜像重放缓冲 Fill 时据此丢弃，
                // 避免"快照已含 + 重放再加"的双计（见 Reconciler 的 pending_fills）。
                let request_ts = now_ms();
                let positions = client.fetch_positions().await.map_err(|e| {
                    ExchangeError::Other(format!(
                        "无法获取 {exchange} 的初始持仓基线，拒绝启动（基线只有一次机会，\
                         错了之后无从纠正）: {e}"
                    ))
                })?;
                let position_map: HashMap<Symbol, crate::domain::Position> =
                    positions.into_iter().map(|p| (p.symbol.clone(), p)).collect();
                let local_ts = now_ms();
                for (ex, symbol) in exchange_symbols.iter().filter(|(e, _)| *e == exchange) {
                    let pos =
                        position_map.get(symbol).cloned().unwrap_or(crate::domain::Position {
                            exchange: *ex,
                            symbol: symbol.clone(),
                            size: 0.0,
                        });
                    tracing::info!(%exchange, %symbol, size = pos.size, "Initial position loaded");
                    baselines.insert(
                        (*ex, symbol.clone()),
                        IncomeEvent {
                            exchange_ts: request_ts,
                            local_ts,
                            data: ExchangeEventData::PositionBaseline(pos),
                        },
                    );
                }
            }
        }

        // 6.3 向总线发布**引擎生命周期内首次出现**的 (exchange, symbol) 基线。总线上的基线
        //     只喂引擎生命周期的镜像（对账/观测）—— executor 不经总线收基线（路由层对
        //     PositionBaseline 是 SkipExecutors），它们走 6.4 的点对点。已发布过的 pair
        //     （降级后再晋升）不重发：镜像的「基线 + Fill」账本在实例撤下期间也一直在跟
        //     Fill，重发会被判成"重复基线"违约。紧跟 6.1 发布是为了把「镜像已注册、基线
        //     未到」的窗口压到几次邮箱投递，残余窗口由对账层的 Fill 缓冲重放兜住。
        for ((exchange, symbol), event) in &baselines {
            if self.baselined_positions.contains(&(*exchange, symbol.clone())) {
                continue;
            }
            // 发布失败与拉取失败等价 —— 基线没送达就是基线缺失，同样只有这一次机会
            self.income_pubsub
                .tell(kameo_actors::pubsub::Publish(event.clone()))
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "发布 {exchange}/{symbol} 的持仓基线失败（基线只有一次机会）: {e}"
                    ))
                })?;
            self.baselined_positions.insert((*exchange, symbol.clone()));
        }

        // 6.4 点对点把基线投进本批每个**实盘** executor 的邮箱。**必须先于 6.5 注册**：
        //     邮箱 FIFO 保证基线先于任何流事件被处理 —— "Fill 早于基线到达、基线被判重复
        //     而丢掉存量"的竞态从结构上不可能发生（这正是不让 executor 从总线收基线的
        //     原因：总线上基线与私有流的先后无从保证）。模拟账户从零起步，不需要也不应该
        //     看到真实持仓，不投。
        //
        //     如实声明残余窗口：某笔成交若已含在 REST 快照里、其 Fill 又在注册**之后**才
        //     送达，executor 会双计（快照一次 + Fill 一次）。没有交易所侧序号无法根除，
        //     窗口只有 REST 在途时长。注意该偏差**不在对账覆盖范围内**：对账比对的是
        //     镜像（它按快照请求时刻过滤了重放）vs 交易所读数，executor 自己的偏差没有
        //     通道检出。
        for ((executor_ref, account), subscriptions) in
            executor_refs.iter().zip(strategy_subscriptions.iter())
        {
            if *account != AccountId::Live {
                continue;
            }
            let pairs: HashSet<(Exchange, Symbol)> = subscriptions
                .iter()
                .map(|(ex, kind)| (*ex, kind.symbol().clone()))
                .collect();
            for (exchange, symbol) in pairs {
                // 缺基线只有一种来路：该所未配置 client（与旧行为一致，无基线可投）
                let Some(event) = baselines.get(&(exchange, symbol.clone())) else {
                    continue;
                };
                executor_ref.tell(event.clone()).send().await.map_err(|e| {
                    ExchangeError::Other(format!(
                        "向策略实例投递 {exchange}/{symbol} 的持仓基线失败: {e}"
                    ))
                })?;
            }
        }

        // 6.5 向 ProcessorActor 注册各 Executor 的订阅（自此它们开始收流事件）
        for ((executor_ref, account), subscriptions) in
            executor_refs.iter().zip(strategy_subscriptions.iter())
        {
            // 注册失败必须**向上传播**，不能只打日志：注册不上的实例收不到任何事件
            // （行情、成交全无），却仍会被 push 进 self.executors 并让本函数返回 Ok ——
            // SupervisorActor 据此认定晋升成功、置 live=Some，此后既不会重试也不会 demote，
            // 留下一个"活着却永远收不到事件"的僵尸实例。与"返回 Ok 即已完整投产"的契约相悖。
            self.income_processor
                .tell(RegisterExecutor {
                    executor: executor_ref.clone(),
                    subscriptions: subscriptions.clone(),
                    account: account.clone(),
                })
                .send()
                .await
                .map_err(|e| {
                    ExchangeError::Other(format!(
                        "向 IncomeProcessor 注册策略实例失败（该实例将收不到任何事件）: {e}"
                    ))
                })?;
            self.executors.push(RegisteredExecutor {
                executor: executor_ref.clone(),
                account: account.clone(),
                symbols: subscriptions.iter().map(|(_, k)| k.symbol().clone()).collect(),
            });
        }

        // 6.6 批量向各 ExchangeActors 发送订阅请求（市场数据从此处开始流动）
        for (exchange, kinds) in exchange_subscriptions {
            if let Some(actor) = self.exchange_actors.get(&exchange) {
                actor
                    .subscribe_batch(kinds)
                    .await
                    .map_err(ExchangeError::Other)?;
            }
        }

        Ok(())
    }

    /// 撤下本次刚注册的 executor，使"`AddStrategies` 返回 `Err`"等价于"没有实例在跑"。
    ///
    /// # 回滚的语义边界：只保证不留下无人看管的实例，**不是**恢复原状
    ///
    /// 下列副作用**有意不回滚**，各有理由：
    ///
    /// - **已发出的 `RegisterSymbols`**：对账层比对的是**事件流镜像**，与 executor 无关。
    ///   即便策略没加上，持仓漂移仍然被监控 —— 撤掉反而是削弱安全网。观测层同理，多跟踪
    ///   几个 symbol 只是多几行日志。
    /// - **已推入的持仓基线**：同上，进的是对账层镜像；而被 kill 的 executor 的状态随它消失。
    /// - **已撤掉的遗留挂单**：不可逆，**也不该逆** —— 撤掉本身就是我们要的结果。
    /// - **已发出的行情订阅**：多订几路行情无害（没有订阅者就是丢弃）；而且别的策略实例可能
    ///   订了同一路，贸然退订会把它们的行情一起掐掉。
    ///
    /// 这条边界必须写明：否则后来者会把"回滚不彻底"当成缺陷去补，反而补出问题。
    async fn rollback_executors(&mut self, created: &[(ActorRef<ExecutorActor>, AccountId)]) {
        let ids: HashSet<ActorId> = created.iter().map(|(r, _)| r.id()).collect();
        for (executor, account) in created {
            if let Err(e) = self
                .income_processor
                .tell(UnregisterExecutor {
                    executor: executor.clone(),
                })
                .send()
                .await
            {
                tracing::error!(error = %e, "回滚时注销 executor 失败");
            }
            if let Err(reason) = self.stop_executor(executor).await {
                tracing::error!(%account, %reason, "回滚时未能确认实例已停止");
            }
            tracing::warn!(%account, "投产失败，已撤下该策略实例");
        }
        // self.executors 里可能已被 6.1 推入，一并摘掉（未推入的 retain 是 no-op）
        self.executors
            .retain(|reg| !ids.contains(&reg.executor.id()));
    }
}

// ============================================================================
// 交易所装配：每所一个 setup_*，manager 只做收集与循环
// ============================================================================

/// 交易所 WS actor 装配的执行环境（metas 预加载完成后可用）
struct SpawnCtx {
    manager: ActorRef<ManagerActor>,
    income_pubsub: ActorRef<IncomePubSub>,
    /// 该所的 symbol -> meta
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// 装配产物：类型擦除的操作接口 + 停机句柄。
///
/// 句柄必须在这里造：出了这个闭包，actor 就只剩 `Box<dyn ExchangeActorOps>`，
/// 拿不到具体的 `ActorRef<A>`，也就造不出"停它并等它收尾完"的闭包了。
type SpawnFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(Box<dyn ExchangeActorOps>, ChildStop), ExchangeError>>
            + Send,
    >,
>;

/// 单个交易所的装配产物。
///
/// 装配天然分两阶段：**建 client** 必须先于 symbol metas 预加载（预加载要用它），
/// **spawn WS actor 集**必须后于预加载（actor 要用 metas）。每所把两阶段封进一个
/// `setup_*` 函数：阶段 1 立即执行产出 client，阶段 2 以闭包延迟到 metas 就绪。
///
/// **加一个新交易所只需**：写一个 `setup_*`、`ManagerActorArgs` 加一个配置字段、
/// `on_start` 里加一行 push —— 此前建 client / spawn / join 等待 / 插 map 四段都是
/// 逐所硬编码，加一个所要改六处。
struct ExchangeSetup {
    exchange: Exchange,
    client: Arc<dyn ExchangeClient>,
    /// 有凭证 = 有账户（决定是否跑持仓轮询/对账）
    authed: bool,
    /// 延迟执行的 actor 装配：spawn + 等 on_start 完成，返回类型擦除的操作接口。
    /// 各 setup 的 future 由 on_start 并发 join —— spawn 瞬间返回，等待是并发的。
    spawn_actor: Box<dyn FnOnce(SpawnCtx) -> SpawnFuture + Send>,
}

fn setup_binance(access: ExchangeAccess<BinanceCredentials>) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(BinanceClient::new(
        access.quote.clone(),
        access.credentials.clone(),
    )?);
    let authed = access.has_credentials();
    let client_dyn: Arc<dyn ExchangeClient> = client.clone();
    Ok(ExchangeSetup {
        exchange: Exchange::Binance,
        client: client_dyn.clone(),
        authed,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = BinanceActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    BinanceActorArgs {
                        credentials: access.credentials,
                        symbol_metas: ctx.symbol_metas,
                        rest_base_url: REST_BASE_URL.to_string(),
                        income_pubsub: ctx.income_pubsub,
                        client: client_dyn,
                        quote: access.quote,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor.wait_for_startup_result().await.map_err(|e| {
                    ExchangeError::Other(format!("BinanceActor failed to start: {e}"))
                })?;
                let stop = ChildStop::new("BinanceActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

fn setup_okx(access: ExchangeAccess<OkxCredentials>) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(OkxClient::new(access.quote.clone(), access.credentials.clone())?);
    let authed = access.has_credentials();
    Ok(ExchangeSetup {
        exchange: Exchange::OKX,
        client: client.clone(),
        authed,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = OkxActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    OkxActorArgs {
                        credentials: access.credentials,
                        client: Some(client),
                        symbol_metas: ctx.symbol_metas,
                        income_pubsub: ctx.income_pubsub,
                        quote: access.quote,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor
                    .wait_for_startup_result()
                    .await
                    .map_err(|e| ExchangeError::Other(format!("OkxActor failed to start: {e}")))?;
                let stop = ChildStop::new("OkxActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

fn setup_hyperliquid(
    access: ExchangeAccess<HyperliquidCredentials>,
) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(HyperliquidClient::new(
        access.quote.clone(),
        access.dex.clone(),
        access.credentials.clone(),
    )?);
    let authed = access.has_credentials();
    Ok(ExchangeSetup {
        exchange: Exchange::Hyperliquid,
        client: client.clone(),
        authed,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = HyperliquidActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    HyperliquidActorArgs {
                        credentials: access.credentials,
                        symbol_metas: ctx.symbol_metas,
                        income_pubsub: ctx.income_pubsub,
                        quote: access.quote,
                        dex: access.dex,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor.wait_for_startup_result().await.map_err(|e| {
                    ExchangeError::Other(format!("HyperliquidActor failed to start: {e}"))
                })?;
                let stop = ChildStop::new("HyperliquidActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

/// IBKR 的构造是异步的（要打网关），且配置形状与其他三所不同（无 quote、另带
/// snapshot 轮询配置）—— 差异封在本函数内，manager 不感知。
async fn setup_ibkr(
    cred: IbkrCredentials,
    snapshot: Option<IbkrSnapshotConfig>,
) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(IbkrClient::new(&cred).await?);
    let auth = client.auth();
    let conids = client.conids().clone();
    Ok(ExchangeSetup {
        exchange: Exchange::IBKR,
        client: client.clone(),
        // IBKR 的 client 只在有凭证时才构建，走到这里必然有账户
        authed: true,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = IbkrActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    IbkrActorArgs {
                        auth,
                        income_pubsub: ctx.income_pubsub,
                        conids,
                        client,
                        snapshot,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor
                    .wait_for_startup_result()
                    .await
                    .map_err(|e| ExchangeError::Other(format!("IbkrActor failed to start: {e}")))?;
                let stop = ChildStop::new("IbkrActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

impl Actor for ManagerActor {
    type Args = ManagerActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 装配各配置了的交易所（阶段 1：建 client；阶段 2 的 actor 装配延迟到
        //    metas 就绪后并发执行）。加新交易所：写 setup_*、args 加字段、这里加一行。
        let mut setups: Vec<ExchangeSetup> = Vec::new();
        if let Some(access) = args.binance {
            setups.push(setup_binance(access)?);
        }
        if let Some(access) = args.okx {
            setups.push(setup_okx(access)?);
        }
        if let Some(access) = args.hyperliquid {
            setups.push(setup_hyperliquid(access)?);
        }
        if let Some(cred) = args.ibkr_credentials {
            setups.push(setup_ibkr(cred, args.ibkr_snapshot).await?);
        }

        //    模拟盘也需要 client：symbol metas 走公共 REST，与是否有凭证无关
        let clients: HashMap<Exchange, Arc<dyn ExchangeClient>> =
            setups.iter().map(|s| (s.exchange, s.client.clone())).collect();
        // 有凭证 = 有账户可对账；只接公共行情的所没有持仓可言，不必轮询
        let authed_exchanges: HashSet<Exchange> =
            setups.iter().filter(|s| s.authed).map(|s| s.exchange).collect();

        // 2. 预加载所有交易所的 symbol metas
        let symbol_metas = Self::preload_all_symbol_metas(&clients).await?;

        // 3. 创建 PubSub Actors (使用 spawn_link_with_mailbox)
        let mut pipeline = ChildGroup::default();
        let mut producers = ChildGroup::default();
        let income_pubsub = pipeline.spawn::<IncomePubSub, _>(
            &actor_ref,
            "IncomePubSub",
            IncomePubSub::new(DeliveryStrategy::BestEffort),
        )
        .await;
        let outcome_pubsub = pipeline.spawn::<OutcomePubSub, _>(
            &actor_ref,
            "OutcomePubSub",
            OutcomePubSub::new(DeliveryStrategy::BestEffort),
        )
        .await;

        // 4. 创建 ProcessorActor 并订阅 income_pubsub
        let processor = pipeline.spawn::<IncomeProcessorActor, _>(
            &actor_ref,
            "IncomeProcessorActor",
            IncomeProcessorActor::default(),
        )
        .await;
        income_pubsub
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
            income_pubsub.clone(),
            // 出向单位折算在此完成（见 ExchangeOrder）；策略与回测全程币本位
            Arc::new(symbol_metas.clone()),
            false,
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
        // （见 OutcomeProcessorActor::in_flight）。只订 OrderUpdate，不吃行情洪水。
        income_pubsub
            .tell(SubscribeFilter(live_processor, |e: &IncomeEvent| {
                matches!(e.data, ExchangeEventData::OrderUpdate(_))
            }))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 模拟账户私有事件走独立总线：结构上保证真实账户的成交不可能流进模拟策略的状态
        let paper_pubsub = pipeline.spawn::<PaperPubSub, _>(
            &actor_ref,
            "PaperPubSub",
            PaperPubSub::new(DeliveryStrategy::BestEffort),
        )
        .await;
        let paper_counter = pipeline.spawn::<PaperCounterActor, _>(
            &actor_ref,
            "PaperCounterActor",
            PaperCounterArgs {
                paper_pubsub: paper_pubsub.clone(),
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
        // 柜台需要共享行情（撮合）与 Clock（发布净值）
        income_pubsub
            .tell(Subscribe(paper_counter.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        // **柜台的回报要能回到策略**：IncomeProcessor 订阅 paper 总线，按账户投递给对应
        // executor。漏掉这一步的表现是"订单永远停在 Created、超时被清理后反复重挂"——
        // 策略收不到 Pending/Filled，看不见自己的挂单。
        paper_pubsub
            .clone()
            .tell(Subscribe(processor.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5.5 创建 MetricsActor 并订阅 income_pubsub（观测层：只读事件流，不参与决策）
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
        income_pubsub
            .tell(Subscribe(metrics.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        // 模拟账户的成交/拒单/净值也要进指标（此前是观测盲区）；MetricsActor 内部
        // 按账户分账，绝不与实盘视图混淆
        paper_pubsub
            .tell(Subscribe(metrics.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5.6 持仓对账（通道 B）：REST 读数 vs 本地「基线 + Fill」。
        //     与 MetricsActor 分开是刻意的——观测层故障不该拖垮交易，而对账确认漂移必须
        //     致命退出（本地持仓不可信时，策略的三道风控闸门全部失效）。
        let position_reconciler = pipeline.spawn::<PositionReconcileActor, _>(
            &actor_ref,
            "PositionReconcileActor",
            PositionReconcileArgs {
                symbol_metas: Arc::new(symbol_metas.clone()),
                max_consecutive_mismatches: DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
            },
        )
        .await;
        income_pubsub
            .tell(Subscribe(position_reconciler.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        //     读数的产出者：每个**有凭证**的所一个通用 poller。`fetch_positions` 在
        //     ExchangeClient trait 上，故这里是个循环而不是逐所硬编码，交易所模块无需改动。
        for exchange in &authed_exchanges {
            let Some(client) = clients.get(exchange) else {
                continue;
            };
            producers.spawn::<PositionPollingActor, _>(
                &actor_ref,
            "PositionPollingActor",
                PositionPollingActorArgs {
                    client: client.clone(),
                    income_pubsub: income_pubsub.clone(),
                    interval_ms: DEFAULT_POSITION_POLL_INTERVAL_MS,
                },
            )
            .await;
        }

        // 6. 创建 ClockActor（发布 Clock 事件到 income_pubsub）
        producers.spawn::<ClockActor, _>(
            &actor_ref,
            "ClockActor",
            ClockActorArgs {
                interval_ms: 1000,
                income_pubsub: income_pubsub.clone(),
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
                    income_pubsub: income_pubsub.clone(),
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
            income_pubsub,
            outcome_pubsub,
            income_processor: processor,
            metrics,
            position_reconciler,
            exchange_actors,
            paper_pubsub,
            order_gateway,
            executors: Vec::new(),
            producers,
            pipeline,
            baselined_positions: HashSet::new(),
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
pub struct SubscribeIncome<A: Actor> {
    actor: Supervised<A>,
    filter: fn(&IncomeEvent) -> bool,
}

impl<A: Actor> SubscribeIncome<A> {
    /// 订阅全部事件
    pub fn all(actor: Supervised<A>) -> Self {
        Self { actor, filter: |_| true }
    }

    /// 只订阅 `filter` 返回 true 的事件。
    ///
    /// 过滤在 PubSub 自己的任务里执行：不通过的事件**连 clone 都省掉**，订阅者也不会
    /// 被唤醒。观测类订阅者往往只关心少数几种事件（如告警只看订单回报与成交），全收等于每个行情 tick 都白唤醒一次。
    ///
    /// `filter` 是 `fn` 指针不是闭包，**不能捕获**任何东西 —— 判据必须只看事件本身，
    /// 这样"我订了什么"是一段可以直接读懂的静态声明，而不是运行期才知道的行为。
    pub fn only(actor: Supervised<A>, filter: fn(&IncomeEvent) -> bool) -> Self {
        Self { actor, filter }
    }
}

impl<A> Message<SubscribeIncome<A>> for ManagerActor
where
    A: Actor + Message<crate::messaging::IncomeEvent>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeIncome<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self
            .income_pubsub
            .tell(SubscribeFilter(msg.actor.into_inner(), msg.filter))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish to IncomePubSub");
        }
    }
}

/// 外部向策略注入自定义事件（[`crate::messaging::CustomEvent`] 的**入向**入口）。
///
/// 事件进入 Income 总线后按 scope 路由：带 `(exchange, symbol)` 的只投递给订阅了
/// 该 symbol 的策略，无 scope 的广播 —— 与行情同一套分发，无第二条通道。策略在
/// `on_event` 里 match `ExchangeEventData::Custom` 并按类型 `get::<T>()` 消费。
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
        let event = IncomeEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: ExchangeEventData::Custom(msg.0),
        };
        if let Err(e) = self
            .income_pubsub
            .tell(kameo_actors::pubsub::Publish(event))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish custom event to IncomePubSub");
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

/// 撤下某账户在指定 symbol 上的策略实例，可选立即平仓。
///
/// 用于降级：`live 持续亏损 -> 关闭 live`。两步都必要 ——
/// 只撤实例不平仓会留下无人看管的敞口；只平仓不撤实例会被策略立刻重新开仓。
pub struct RemoveStrategies {
    /// 目标账户（降级即 [`AccountId::Live`]）
    pub account: AccountId,
    /// 目标 symbol
    pub symbols: Vec<Symbol>,
    /// 平仓下到哪个交易所
    pub exchange: Exchange,
    /// 是否立即市价平掉存量仓位
    pub flatten: bool,
}

impl Message<RemoveStrategies> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: RemoveStrategies,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let targets: Vec<Symbol> = msg.symbols;
        // 交易所侧的残留（挂单 / 仓位）只有实盘账户才有：模拟账户的两者都在本地柜台里，
        // 随账户一起留存（供继续记账）
        let is_live = msg.account == AccountId::Live;
        let need_flatten = msg.flatten && is_live;

        // 1. 先撤实例，再平仓。顺序不能反：先平仓的话，策略在收到平仓回报后可能立刻重开。
        //
        //    平仓量取自 executor 的**本地持仓**，且必须在 `kill` 之前问 —— kill 之后它不再
        //    消费事件、状态就停更了。不走 REST：持仓由「基线 + Fill」维护，REST 在本项目里
        //    只作对账（见 PositionReconcileActor）；且本地值比 REST 快照更新（后者可能落后于
        //    最新到达的 Fill）。真出现本地与交易所不一致时，对账层会先致命退出，走不到这里。
        let mut removed = 0usize;
        let mut kept = Vec::new();
        // 按 (所, symbol) 去重：同一 symbol 若有多个实例，它们看到的是同一份交易所持仓，
        // 累加会把平仓量翻倍（reduce-only 虽能兜住，但下单量本身就该是对的）
        let mut to_flatten: HashMap<(Exchange, Symbol), f64> = HashMap::new();
        // 未能完成的部分：**必须向调用方传播**。SupervisorActor 收到 Ok 就会把 live 置为
        // None、认为已干净降级，而实盘可能仍有未平的仓位且此后无人看管 —— 那正是
        // supervisor 自己写下的"谎报已关闭比撤下失败更危险"。
        let mut incomplete: Vec<String> = Vec::new();
        for reg in std::mem::take(&mut self.executors) {
            let hit = reg.account == msg.account
                && targets.iter().any(|s| reg.symbols.contains(s));
            if !hit {
                kept.push(reg);
                continue;
            }
            // **拒绝部分覆盖**：实例覆盖的 symbol 必须完全落在 targets 内。
            //
            // 撤下一个实例就是停掉它管的**所有** symbol，而本次只会为 targets 撤单、平仓。
            // 若实例还管着 targets 之外的 symbol，它们的挂单不撤、仓位不平，却再无实例维护
            // —— 恰好制造出本次改动通篇要消灭的"无人看管的仓位"。而且对账层也发现不了：
            // 基线已建、Fill 不再到达，只要交易所侧不动就永远不告警。
            //
            // 扩大范围去撤/平那些 symbol 同样不对：调用方没要求动它们，静默清仓更意外。
            // 故拒绝并如实报错，让调用方把该实例的 symbol 一次性降完。
            let uncovered: Vec<&Symbol> =
                reg.symbols.iter().filter(|s| !targets.contains(s)).collect();
            if !uncovered.is_empty() {
                tracing::error!(
                    account = %reg.account,
                    ?targets,
                    ?uncovered,
                    "拒绝部分降级：该实例还管着 targets 之外的 symbol，撤下它会让那些 symbol \
                     无人看管；请一次性降完该实例的全部 symbol"
                );
                incomplete.push(format!(
                    "实例覆盖 {uncovered:?} 不在本次 targets 内，拒绝部分降级"
                ));
                kept.push(reg);
                continue;
            }
            if need_flatten {
                match reg
                    .executor
                    .ask(GetPositions(targets.clone()))
                    .send()
                    .await
                {
                    Ok(positions) => {
                        for pos in positions {
                            to_flatten.insert((pos.exchange, pos.symbol), pos.size);
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "取 executor 本地持仓失败，该实例的仓位无法平掉"
                        );
                        incomplete.push(format!("取本地持仓失败: {e}"));
                    }
                }
            }
            if let Err(e) = self
                .income_processor
                .tell(UnregisterExecutor {
                    executor: reg.executor.clone(),
                })
                .send()
                .await
            {
                tracing::error!(error = %e, "Failed to unregister executor");
            }
            // 有序停下并**等它真的停**：后面要撤单 / 平仓，不能让它在排空邮箱时又下新单
            if let Err(reason) = self.stop_executor(&reg.executor).await {
                tracing::error!(
                    account = %reg.account,
                    %reason,
                    "未能确认实例已停止 —— 它可能仍在产出订单"
                );
                incomplete.push(format!("实例未确认停止: {reason}"));
            }
            removed += 1;
        }
        self.executors = kept;
        tracing::info!(
            account = %msg.account,
            symbols = ?targets,
            removed,
            "Removed strategy instances"
        );

        // 模拟账户到此为止：它的挂单与仓位都在本地柜台里，随账户一起留存（供继续记账），
        // 交易所侧没有任何残留需要清理。
        //
        // **但仍要走 incomplete 检查**：撤下阶段本身可能已经失败（拒绝部分降级、实例未确认
        // 停止），在这里直接 `Ok(())` 会把"实例还在跑"谎报成"已干净撤下" —— 正是本分支通篇
        // 要消灭的那类谎报。
        if !is_live {
            return Self::finish_removal(&msg.account, msg.exchange, incomplete);
        }
        let Some(client) = self.clients.get(&msg.exchange).cloned() else {
            return Err(ExchangeError::Other(format!(
                "No client for {} — 既无法撤单也无法平仓",
                msg.exchange
            )));
        };

        // 2. 撤掉遗留挂单。**位置是关键**：
        //
        //    - 必须在 `kill` **之后**：实例还活着时撤单，它会收到 Cancelled 回报，
        //      gamma_scalp 那类"看到 pending 为空就补挂"的策略会立刻重新挂上来
        //      （与上面"先撤实例再平仓"是同一个理由）；
        //    - 必须在平仓**之前**：否则 reduce-only 平仓单下去之后，那张遗留的开仓单还可能
        //      成交，又开出一个新仓位。
        //
        //    与 `flatten` 标志无关，只要是实盘账户就撤：不平仓可能是"想留着仓位人工处理"，
        //    但留一张**会自己再开仓**的挂单没有任何合理理由 —— 那正是降级要消除的东西。
        for symbol in &targets {
            if let Err(e) = Self::cancel_leftover_orders(&client, msg.exchange, symbol).await {
                tracing::error!(
                    exchange = %msg.exchange,
                    %symbol,
                    error = %e,
                    "[DEMOTE] 遗留挂单未撤净 —— 它可能成交并重新开出无人看管的仓位"
                );
                incomplete.push(format!("{symbol}: 遗留挂单未撤净 ({e})"));
            }
        }

        // 3. 平仓。`to_flatten` 只在 `need_flatten` 为真时才被填充，故此处不再判一次；
        //    用断言把这条不变量钉住 —— 将来若有人为别的目的（如日志）无条件填充它，
        //    平仓就会在没被要求时触发。
        debug_assert!(
            need_flatten || to_flatten.is_empty(),
            "未要求平仓却收集了待平仓位"
        );
        for ((exchange, symbol), size) in to_flatten {
            let pos = crate::domain::Position {
                exchange,
                symbol,
                size,
                // 平仓只需要方向与数量；均价/浮盈在此无意义，不伪造
            };
            // 只平下到本次指定交易所的那些腿；空仓判据用 domain 的口径，不另写一份比较
            if exchange != msg.exchange || pos.is_empty() {
                continue;
            }
            // 反向 reduce-only 市价单：撮合层强制不反向开仓，量算多了也只会平到 0
            let side = if pos.size > 0.0 { Side::Short } else { Side::Long };
            let order = Order {
                id: String::new(),
                exchange: msg.exchange,
                symbol: pos.symbol.clone(),
                side,
                order_type: OrderType::Market,
                quantity: pos.size.abs(),
                reduce_only: true,
                client_order_id: msg.exchange.new_cli_order_id(),
            };
            tracing::warn!(
                exchange = %msg.exchange,
                symbol = %pos.symbol,
                position = pos.size,
                %side,
                "[DEMOTE] Flattening live position with reduce-only market order"
            );
            // 经**唯一下单出口**下发（缺 meta、残量不足最小可交易量等失败都从这里
            // 如实返回）。`Ok` 已含两种"目标已达成"情形：reduce-only 因仓位已平被拒、
            // REST 失败但交易所已回报 —— 见 OrderGateway::place 的返回值契约。
            let symbol = pos.symbol.clone();
            if let Err(reason) = self.order_gateway.place(order, "demote_flatten").await {
                tracing::error!(
                    %symbol,
                    position = pos.size,
                    %reason,
                    "平仓下单失败 —— 实盘仍有敞口"
                );
                incomplete.push(format!("{symbol}: 平仓下单失败 ({reason})"));
            }
        }

        Self::finish_removal(&msg.account, msg.exchange, incomplete)
    }
}

impl ManagerActor {
    /// 汇总撤下结果：只要有任何一环没完成，就**向上报错**。
    ///
    /// 调用方（`SupervisorActor::demote`）收到 `Ok` 就会把 `live` 置为 `None`、认为已干净
    /// 降级；若此时实例还在跑、或仓位没平、或遗留挂单没撤掉，那部分敞口就此无人看管 ——
    /// 正是 supervisor 自己写下的"谎报已关闭比撤下失败更危险"。
    fn finish_removal(
        account: &AccountId,
        exchange: Exchange,
        incomplete: Vec<String>,
    ) -> Result<(), ExchangeError> {
        if incomplete.is_empty() {
            return Ok(());
        }
        Err(ExchangeError::Other(format!(
            "撤下未完成（账户 {account}，交易所 {exchange}），需人工核对: {}",
            incomplete.join("; ")
        )))
    }
}

/// 订阅模拟账户私有事件总线（Supervisor / 观测层用）。
/// 退订方式见 [`SubscribeIncome`]（订阅者停机即自动摘除）。
pub struct SubscribePaper<A: Actor> {
    actor: Supervised<A>,
    filter: fn(&AccountIncome) -> bool,
}

impl<A: Actor> SubscribePaper<A> {
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
    pub fn only(actor: Supervised<A>, filter: fn(&AccountIncome) -> bool) -> Self {
        Self { actor, filter }
    }
}

impl<A> Message<SubscribePaper<A>> for ManagerActor
where
    A: Actor + Message<AccountIncome>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribePaper<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self
            .paper_pubsub
            .tell(SubscribeFilter(msg.actor.into_inner(), msg.filter))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to subscribe to PaperPubSub");
        }
    }
}

#[cfg(test)]
mod leftover_order_tests {
    use super::*;
    use crate::domain::{AccountInfo, OrderId, OrderStatus, Position};
    use crate::exchange::ExchangeOrder;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    /// 只实现本组测试用到的方法；其余按"不该被调用"处理 —— 若被调用说明被测逻辑越界了，
    /// 让它显式失败比返回一个假值更好。
    /// 已发出的撤单记录，与 client 共享一份，供断言读取
    type CancelLog = Arc<Mutex<Vec<OrderId>>>;

    struct FakeClient {
        /// 每次 `fetch_pending_orders` 依次返回一份；用于模拟"撤单前 / 撤单后复查"两次调用
        pending: Mutex<VecDeque<Vec<OrderUpdate>>>,
        cancelled: CancelLog,
    }

    impl FakeClient {
        fn new(rounds: Vec<Vec<OrderUpdate>>) -> (Arc<dyn ExchangeClient>, CancelLog) {
            let cancelled: CancelLog = Arc::new(Mutex::new(Vec::new()));
            let client = Arc::new(Self {
                pending: Mutex::new(rounds.into()),
                cancelled: cancelled.clone(),
            });
            (client, cancelled)
        }
    }

    #[async_trait::async_trait]
    impl ExchangeClient for FakeClient {
        fn exchange(&self) -> Exchange {
            EX
        }
        async fn fetch_pending_orders(
            &self,
            _symbol: &Symbol,
        ) -> Result<Vec<OrderUpdate>, ExchangeError> {
            Ok(self.pending.lock().unwrap().pop_front().unwrap_or_default())
        }
        async fn cancel_order(
            &self,
            _symbol: &Symbol,
            order_id: &OrderId,
        ) -> Result<(), ExchangeError> {
            self.cancelled.lock().unwrap().push(order_id.clone());
            Ok(())
        }
        async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
            unreachable!("被测逻辑不该查 symbol metas")
        }
        async fn fetch_symbol_meta(
            &self,
            _symbols: &[Symbol],
        ) -> Result<Vec<SymbolMeta>, ExchangeError> {
            unreachable!("被测逻辑不该查 symbol meta")
        }
        async fn place_order(&self, _order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
            unreachable!("撤单流程不该下单")
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!("撤单流程不该查账户")
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            unreachable!("撤单流程不该查持仓")
        }
    }

    fn order(client_order_id: &str, order_id: &str) -> OrderUpdate {
        OrderUpdate {
            order_id: order_id.to_string(),
            client_order_id: Some(client_order_id.to_string()),
            exchange: EX,
            symbol: SYM.to_string(),
            side: Side::Long,
            status: OrderStatus::Pending,
            quantity: 1.0,
        }
    }

    /// 无 client_order_id 的挂单（人工 UI 单 / 交易所系统单）
    fn anonymous_order(order_id: &str) -> OrderUpdate {
        OrderUpdate {
            client_order_id: None,
            ..order("", order_id)
        }
    }

    /// 没有本引擎的挂单时，一个撤单请求都不该发
    #[tokio::test]
    async fn nothing_to_cancel_sends_no_request() {
        let (client, cancelled) = FakeClient::new(vec![vec![]]);
        ManagerActor::cancel_leftover_orders(&client, EX, &SYM.to_string())
            .await
            .expect("空列表应直接通过");
        assert!(cancelled.lock().unwrap().is_empty());
    }

    /// **只撤本引擎的单**：人工单 / 其他程序的单 / 无 cloid 的单都不能动。
    /// 撤错别人的挂单是不可接受的。
    #[tokio::test]
    async fn only_own_orders_are_cancelled() {
        let own = EX.new_cli_order_id();
        let foreign = vec![
            order("web_1a2b", "ex-foreign-1"),
            order("android_9f8e", "ex-foreign-2"),
            anonymous_order("ex-anon"),
        ];
        let mut first = foreign.clone();
        first.push(order(&own, "ex-own"));

        // 复查时只剩外部单 —— 本引擎的那张已撤掉
        let (client, cancelled) = FakeClient::new(vec![first, foreign]);
        ManagerActor::cancel_leftover_orders(&client, EX, &SYM.to_string())
            .await
            .expect("外部单残留不该导致失败");

        assert_eq!(
            *cancelled.lock().unwrap(),
            vec!["ex-own".to_string()],
            "只应撤掉本引擎的那一张"
        );
    }

    /// 复查仍有本引擎的挂单 -> 拒绝启动/降级。
    ///
    /// 留着无人看管的挂单开跑，策略会在它旁边重复挂单；对降级而言它还可能成交、
    /// 重新开出一个没人管的仓位。
    #[tokio::test]
    async fn still_present_after_cancel_is_an_error() {
        let own = EX.new_cli_order_id();
        let leftover = vec![order(&own, "ex-own")];
        // 两次都返回同一张 —— 模拟撤不掉
        let (client, _cancelled) = FakeClient::new(vec![leftover.clone(), leftover]);

        let err = ManagerActor::cancel_leftover_orders(&client, EX, &SYM.to_string())
            .await
            .expect_err("撤不掉必须报错");
        assert!(err.to_string().contains("ex-own"), "错误应指明是哪张单: {err}");
    }
}

#[cfg(test)]
mod stop_semantics_tests {
    use super::*;
    use kameo::error::Infallible;
    use std::sync::Mutex;

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

#[cfg(test)]
mod subscription_validation_tests {
    use super::*;

    fn bbo(ex: Exchange) -> (Exchange, SubscriptionKind) {
        (ex, SubscriptionKind::BBO { symbol: "BTC".to_string() })
    }
    fn candle(ex: Exchange) -> (Exchange, SubscriptionKind) {
        (
            ex,
            SubscriptionKind::Candle {
                symbol: "BTC".to_string(),
                interval: crate::domain::CandleInterval::Min1,
            },
        )
    }

    /// 构造只含 (Binance, BTC) 的元数据表（订阅校验的第三个判据）
    fn metas_with_btc() -> HashMap<(Exchange, Symbol), SymbolMeta> {
        let meta = SymbolMeta {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            price_formatter: Arc::new(crate::exchange::utils::StepFormatter::new(0.1)),
            size_step: 0.001,
            min_order_size: 0.001,
            contract_size: 1.0,
        };
        [((Exchange::Binance, "BTC".to_string()), meta)]
            .into_iter()
            .collect()
    }

    /// 未配置的所、不支持的 kind，都在投产期拒绝 —— 不能等上线后"一直没数据"才发现
    #[test]
    fn unavailable_exchange_and_unsupported_kind_are_rejected() {
        let available: HashSet<Exchange> = [Exchange::Binance].into_iter().collect();
        let metas = metas_with_btc();
        // OKX 未配置
        let err = validate_subscriptions(&[bbo(Exchange::OKX)].into_iter().collect(), &available, &metas)
            .expect_err("未配置的所必须拒绝");
        assert!(err.contains("未配置"), "got: {err}");
        // Binance 不支持 Candle
        let err = validate_subscriptions(&[candle(Exchange::Binance)].into_iter().collect(), &available, &metas)
            .expect_err("不支持的 kind 必须拒绝");
        assert!(err.contains("不支持"), "got: {err}");
        // 合法订阅放行
        validate_subscriptions(&[bbo(Exchange::Binance)].into_iter().collect(), &available, &metas)
            .expect("已配置且支持的订阅应放行");
    }

    /// **Critical 回归防线**：订阅了没有 SymbolMeta 的 symbol 必须拒绝投产。
    ///
    /// 各所的元数据加载都会剔除字段异常的合约。被剔掉的 symbol 若正好是策略要交易的，
    /// 故障会以三种不相干的面貌分散出现：下单被 "No SymbolMeta" 拒、私有回报因无法折算
    /// 张数被丢弃（持仓从此落后于交易所）、行情订阅被忽略。根因只有一个，且在此刻可查。
    #[test]
    fn subscription_without_symbol_meta_is_rejected() {
        let available: HashSet<Exchange> = [Exchange::Binance].into_iter().collect();
        let eth = (
            Exchange::Binance,
            SubscriptionKind::BBO { symbol: "ETH".to_string() },
        );
        let err = validate_subscriptions(
            &[eth].into_iter().collect(),
            &available,
            &metas_with_btc(),
        )
        .expect_err("缺 SymbolMeta 的订阅必须在投产期拒绝，而不是上线后三处分别失效");
        assert!(err.contains("SymbolMeta"), "错误应指明根因: {err}");
    }

    /// 能力查询的两个**手写例外**（派生部分直接问各所 kind 映射函数，无需测试同步）：
    /// OKX 的 Candle 由 business WS 承接、IBKR 只实现 BBO —— 这两条不在映射函数里，
    /// 靠本测试钉住。
    #[test]
    fn hand_written_capability_exceptions_hold() {
        use crate::exchange::supports_subscription;
        let candle_kind = SubscriptionKind::Candle {
            symbol: "BTC".to_string(),
            interval: crate::domain::CandleInterval::Min1,
        };
        assert!(
            supports_subscription(Exchange::OKX, &candle_kind),
            "OKX 的 Candle 由 business WS 承接，能力查询必须为真"
        );
        assert!(supports_subscription(
            Exchange::IBKR,
            &SubscriptionKind::BBO { symbol: "AAPL".to_string() }
        ));
        assert!(
            !supports_subscription(
                Exchange::IBKR,
                &SubscriptionKind::Trades { symbol: "AAPL".to_string() }
            ),
            "IBKR 只实现了 BBO"
        );
    }
}

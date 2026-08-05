//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 创建 PubSub Actors (IncomePubSub, OutcomePubSub)
//! - 使用 spawn_link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use super::{
    AccountIncome, AccountOutcome, ClockActor, ClockActorArgs, ExecutorActor, ExecutorArgs, IncomePubSub,
    PaperCounterActor, PaperCounterArgs, PaperPubSub,
    GetPositions, IncomeProcessorActor, MetricsActor, MetricsActorArgs, OutcomePubSub,
    OutcomeProcessorActor,
    PositionPollingActor, PositionPollingActorArgs, PositionReconcileActor, PositionReconcileArgs,
    RegisterExecutor, RegisterSymbols, OutcomeProcessorArgs, UnregisterExecutor,
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
    ExchangeAccess, ExchangeActorOps, ExchangeClient, ExchangeOrder, SubscriptionKind,
};
use crate::strategy::Strategy;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Subscribe;
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

    /// IBKR 具体 client (Arc)，供上层策略经 GetIbkrClient 取引用，调用借券费/外汇等
    /// **非 ExchangeClient trait** 的具体方法。None = 未配置 IBKR。单会话复用同一 client。
    ibkr_client: Option<Arc<IbkrClient>>,

    /// 模拟账户私有事件总线（供 Supervisor 等观察者订阅）
    paper_pubsub: ActorRef<PaperPubSub>,

    /// 已注册的策略实例，供动态撤下使用。
    ///
    /// 晋升/降级要求能在运行期增删实例（见 [`RemoveStrategies`]），故必须留存引用；
    /// 否则实例只能随进程生死。
    executors: Vec<RegisteredExecutor>,
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
    async fn stop_executor(executor: &ActorRef<ExecutorActor>) -> Result<(), String> {
        if let Err(e) = executor.stop_gracefully().await {
            return Err(format!("发送 Stop 信号失败: {e}"));
        }
        tokio::time::timeout(EXECUTOR_STOP_TIMEOUT, executor.wait_for_shutdown())
            .await
            .map_err(|_| {
                format!(
                    "等待实例停止超时（{}s）",
                    EXECUTOR_STOP_TIMEOUT.as_secs()
                )
            })
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
            let executor_ref = ExecutorActor::spawn_link_with_mailbox(
                &actor_ref,
                ExecutorArgs {
                    strategy: spec.strategy,
                    account: spec.account,
                    symbol_metas: Arc::new(self.symbol_metas.clone()),
                    outcome_pubsub: outcome_pubsub.clone(),
                },
                mailbox::unbounded(),
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

    /// 把已创建的 executor 注册进事件流并投产：注册订阅 → 注册观测/对账范围 → 推持仓基线
    /// → 放行行情。
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
        let processor = self.income_processor.clone();

        // 6.1 向 ProcessorActor 注册各 Executor 的订阅（自此它们开始收事件）
        for ((executor_ref, account), subscriptions) in
            executor_refs.iter().zip(strategy_subscriptions.iter())
        {
            if let Err(e) = processor
                .tell(RegisterExecutor {
                    executor: executor_ref.clone(),
                    subscriptions: subscriptions.clone(),
                    account: account.clone(),
                })
                .send()
                .await
            {
                tracing::error!(error = %e, "Failed to register executor on IncomeProcessor");
            }
            self.executors.push(RegisteredExecutor {
                executor: executor_ref.clone(),
                account: account.clone(),
                symbols: subscriptions.iter().map(|(_, k)| k.symbol().clone()).collect(),
            });
        }

        // 6.2 告知观测层与对账层要跟踪哪些 symbol。**必须在推持仓基线之前**——否则观测层会
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
            if let Err(e) = self.metrics.ask(RegisterSymbols(symbols.clone())).send().await {
                tracing::error!(error = %e, "Failed to register symbols on MetricsActor");
            }
            if let Err(e) = self
                .position_reconciler
                .ask(RegisterSymbols(symbols))
                .send()
                .await
            {
                tracing::error!(error = %e, "Failed to register symbols on PositionReconcileActor");
            }
        }

        // 6.3 查询各交易所初始持仓，推到 income_pubsub。**必须在 executor 注册之后、
        //     行情放行之前**进行——之前发布会被 BestEffort 丢，之后才发就有"开仓信号在持仓
        //     未对齐"的窗口。对每个 (exchange, symbol) 都推一条：交易所返回的用真实数据，
        //     未返回的推 size=0 显式清零（保证 SymbolState 一定收到初始值）。
        {
            // 仅实盘账户需要：这些事件走共享 IncomePubSub，只会路由给实盘账户的 executor；
            // 模拟账户从零起步，不需要也不应该看到真实持仓
            let exchanges: HashSet<Exchange> = exchange_symbols.iter().map(|(e, _)| *e).collect();
            for exchange in exchanges {
                let Some(client) = self.clients.get(&exchange) else {
                    continue;
                };
                // 拉取失败即**致命**：持仓基线只有这一次机会，之后全程靠 Fill 累加，
                // 没有第二个来源能纠正它。跳过的后果是策略从"仓位 0"起算——若账户实际有
                // 持仓，三道风控闸门（单边杠杆/账户杠杆/仓位上限）会全部基于错误基线放行。
                // 宁可拒绝启动。
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
                            entry_price: 0.0,
                            unrealized_pnl: 0.0,
                        });
                    tracing::info!(%exchange, %symbol, size = pos.size, "Initial position loaded");
                    let event = IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::PositionBaseline(pos),
                    };
                    if let Err(e) = self
                        .income_pubsub
                        .tell(kameo_actors::pubsub::Publish(event))
                        .send()
                        .await
                    {
                        tracing::error!(error = %e, "Failed to publish initial position");
                    }
                }
            }
        }

        // 6.4 批量向各 ExchangeActors 发送订阅请求（市场数据从此处开始流动）
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
            if let Err(reason) = Self::stop_executor(executor).await {
                tracing::error!(%account, %reason, "回滚时未能确认实例已停止");
            }
            tracing::warn!(%account, "投产失败，已撤下该策略实例");
        }
        // self.executors 里可能已被 6.1 推入，一并摘掉（未推入的 retain 是 no-op）
        self.executors
            .retain(|reg| !ids.contains(&reg.executor.id()));
    }
}

impl Actor for ManagerActor {
    type Args = ManagerActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 创建 Exchange Clients
        //    模拟盘也需要 client：symbol metas 走公共 REST，与是否有凭证无关
        let mut clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = HashMap::new();
        // 有凭证 = 有账户可对账；只接公共行情的所没有持仓可言，不必轮询
        let mut authed_exchanges: HashSet<Exchange> = HashSet::new();

        if let Some(ref access) = args.binance {
            let creds = access.credentials.clone();
            let client = BinanceClient::new(access.quote.clone(), creds)?;
            clients.insert(Exchange::Binance, Arc::new(client));
            if access.has_credentials() {
                authed_exchanges.insert(Exchange::Binance);
            }
        }

        let okx_client: Option<Arc<OkxClient>> = if let Some(ref access) = args.okx {
            let creds = access.credentials.clone();
            let client = Arc::new(OkxClient::new(access.quote.clone(), creds)?);
            clients.insert(Exchange::OKX, client.clone());
            if access.has_credentials() {
                authed_exchanges.insert(Exchange::OKX);
            }
            Some(client)
        } else {
            None
        };

        if let Some(ref access) = args.hyperliquid {
            let creds = access.credentials.clone();
            let client =
                HyperliquidClient::new(access.quote.clone(), access.dex.clone(), creds)?;
            clients.insert(Exchange::Hyperliquid, Arc::new(client));
            if access.has_credentials() {
                authed_exchanges.insert(Exchange::Hyperliquid);
            }
        }

        // IBKR 需要额外保存 auth / conids / client 供 Actor 使用
        let mut ibkr_actor_data = None;
        let mut ibkr_client_ref: Option<Arc<IbkrClient>> = None;
        if let Some(ref cred) = args.ibkr_credentials {
            let ibkr_client = Arc::new(IbkrClient::new(cred).await?);
            ibkr_actor_data = Some((
                ibkr_client.auth(),
                ibkr_client.conids().clone(),
                ibkr_client.clone(),
            ));
            ibkr_client_ref = Some(ibkr_client.clone()); // 具体类型引用，供 GetIbkrClient 外传
            clients.insert(Exchange::IBKR, ibkr_client as Arc<dyn ExchangeClient>);
            // IBKR 的 client 只在有凭证时才构建，走到这里必然有账户
            authed_exchanges.insert(Exchange::IBKR);
        }

        // 2. 预加载所有交易所的 symbol metas
        let symbol_metas = Self::preload_all_symbol_metas(&clients).await?;

        // 3. 创建 PubSub Actors (使用 spawn_link_with_mailbox)
        let income_pubsub = IncomePubSub::spawn_link_with_mailbox(
            &actor_ref,
            IncomePubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        )
        .await;
        let outcome_pubsub = OutcomePubSub::spawn_link_with_mailbox(
            &actor_ref,
            OutcomePubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        )
        .await;

        // 4. 创建 ProcessorActor 并订阅 income_pubsub
        let processor = IncomeProcessorActor::spawn_link_with_mailbox(
            &actor_ref,
            IncomeProcessorActor::default(),
            mailbox::unbounded(),
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
        let live_processor = OutcomeProcessorActor::spawn_link_with_mailbox(
            &actor_ref,
            OutcomeProcessorArgs {
                clients: clients.clone(),
                income_pubsub: income_pubsub.clone(),
                // 出向单位折算在此完成（见 ExchangeOrder）；策略与回测全程币本位
                symbol_metas: Arc::new(symbol_metas.clone()),
                dry_run: false,
            },
            mailbox::unbounded(),
        )
        .await;
        outcome_pubsub
            .tell(Subscribe(live_processor))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 模拟账户私有事件走独立总线：结构上保证真实账户的成交不可能流进模拟策略的状态
        let paper_pubsub = PaperPubSub::spawn_link_with_mailbox(
            &actor_ref,
            PaperPubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        )
        .await;
        let paper_counter = PaperCounterActor::spawn_link_with_mailbox(
            &actor_ref,
            PaperCounterArgs {
                paper_pubsub: paper_pubsub.clone(),
                config: args.paper,
            },
            mailbox::unbounded(),
        )
        .await;
        outcome_pubsub
            .tell(Subscribe(paper_counter.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;
        // 柜台需要共享行情（撮合）与 Clock（发布净值）
        income_pubsub
            .tell(Subscribe(paper_counter))
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
        let metrics = MetricsActor::spawn_link_with_mailbox(
            &actor_ref,
            MetricsActorArgs {
                interval_ms: DEFAULT_REPORT_INTERVAL_MS,
            },
            mailbox::unbounded(),
        )
        .await;
        income_pubsub
            .tell(Subscribe(metrics.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5.6 持仓对账（通道 B）：REST 读数 vs 本地「基线 + Fill」。
        //     与 MetricsActor 分开是刻意的——观测层故障不该拖垮交易，而对账确认漂移必须
        //     致命退出（本地持仓不可信时，策略的三道风控闸门全部失效）。
        let position_reconciler = PositionReconcileActor::spawn_link_with_mailbox(
            &actor_ref,
            PositionReconcileArgs {
                symbol_metas: Arc::new(symbol_metas.clone()),
                max_consecutive_mismatches: DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
            },
            mailbox::unbounded(),
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
            PositionPollingActor::spawn_link_with_mailbox(
                &actor_ref,
                PositionPollingActorArgs {
                    client: client.clone(),
                    income_pubsub: income_pubsub.clone(),
                    interval_ms: DEFAULT_POSITION_POLL_INTERVAL_MS,
                },
                mailbox::unbounded(),
            )
            .await;
        }

        // 6. 创建 ClockActor（发布 Clock 事件到 income_pubsub）
        let _clock_actor = ClockActor::spawn_link_with_mailbox(
            &actor_ref,
            ClockActorArgs {
                interval_ms: 1000,
                income_pubsub: income_pubsub.clone(),
            },
            mailbox::unbounded(),
        )
        .await;

        // 7. 创建所有配置了凭证的 ExchangeActors
        //    Phase 1: 顺序 spawn（每个 spawn_link_with_mailbox 本身瞬间返回）
        //    Phase 2: tokio::join! 并发等所有 on_start 完成（这才是耗时的 IO 部分）
        let binance_ref_opt = if let Some(ref access) = args.binance {
            let symbol_metas_for_exchange =
                Self::get_symbol_metas_for(&symbol_metas, Exchange::Binance);
            let client = clients
                .get(&Exchange::Binance)
                .ok_or_else(|| ExchangeError::Other("Binance client not found".to_string()))?
                .clone();
            Some(
                BinanceActor::spawn_link_with_mailbox(
                    &actor_ref,
                    BinanceActorArgs {
                        credentials: access.credentials.clone(),
                        symbol_metas: symbol_metas_for_exchange,
                        rest_base_url: REST_BASE_URL.to_string(),
                        income_pubsub: income_pubsub.clone(),
                        client,
                        quote: access.quote.clone(),
                    },
                    mailbox::unbounded(),
                )
                .await,
            )
        } else {
            None
        };

        let okx_ref_opt = if let Some(ref access) = args.okx {
            let symbol_metas_for_exchange = Self::get_symbol_metas_for(&symbol_metas, Exchange::OKX);
            Some(
                OkxActor::spawn_link_with_mailbox(
                    &actor_ref,
                    OkxActorArgs {
                        credentials: access.credentials.clone(),
                        client: okx_client.clone(),
                        symbol_metas: symbol_metas_for_exchange,
                        income_pubsub: income_pubsub.clone(),
                        quote: access.quote.clone(),
                    },
                    mailbox::unbounded(),
                )
                .await,
            )
        } else {
            None
        };

        let hyper_ref_opt = if let Some(ref access) = args.hyperliquid {
            let symbol_metas_for_exchange =
                Self::get_symbol_metas_for(&symbol_metas, Exchange::Hyperliquid);
            Some(
                HyperliquidActor::spawn_link_with_mailbox(
                    &actor_ref,
                    HyperliquidActorArgs {
                        credentials: access.credentials.clone(),
                        symbol_metas: symbol_metas_for_exchange,
                        income_pubsub: income_pubsub.clone(),
                        quote: access.quote.clone(),
                        dex: access.dex.clone(),
                    },
                    mailbox::unbounded(),
                )
                .await,
            )
        } else {
            None
        };

        let ibkr_ref_opt = if let Some((ibkr_auth, ibkr_conids, ibkr_client)) = ibkr_actor_data {
            Some(
                IbkrActor::spawn_link_with_mailbox(
                    &actor_ref,
                    IbkrActorArgs {
                        auth: ibkr_auth,
                        income_pubsub: income_pubsub.clone(),
                        conids: ibkr_conids,
                        client: ibkr_client,
                        snapshot: args.ibkr_snapshot,
                    },
                    mailbox::unbounded(),
                )
                .await,
            )
        } else {
            None
        };

        // Phase 2: 并发等四家完成；任一失败 → 向上传播 ExchangeError（启动期受控退出）
        let b_wait = async {
            match &binance_ref_opt {
                Some(r) => r.wait_for_startup_result().await,
                None => Ok(()),
            }
        };
        let o_wait = async {
            match &okx_ref_opt {
                Some(r) => r.wait_for_startup_result().await,
                None => Ok(()),
            }
        };
        let h_wait = async {
            match &hyper_ref_opt {
                Some(r) => r.wait_for_startup_result().await,
                None => Ok(()),
            }
        };
        let i_wait = async {
            match &ibkr_ref_opt {
                Some(r) => r.wait_for_startup_result().await,
                None => Ok(()),
            }
        };
        let (b, o, h, i) = tokio::join!(b_wait, o_wait, h_wait, i_wait);
        // 任一交易所启动失败 → 向上传播（受控退出，不重试/不重连）
        b.map_err(|e| ExchangeError::Other(format!("BinanceActor failed to start: {e}")))?;
        o.map_err(|e| ExchangeError::Other(format!("OkxActor failed to start: {e}")))?;
        h.map_err(|e| ExchangeError::Other(format!("HyperliquidActor failed to start: {e}")))?;
        i.map_err(|e| ExchangeError::Other(format!("IbkrActor failed to start: {e}")))?;

        let mut exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>> = HashMap::new();
        if let Some(r) = binance_ref_opt {
            exchange_actors.insert(Exchange::Binance, Box::new(r));
            tracing::info!(exchange = "Binance", "ExchangeActor ready");
        }
        if let Some(r) = okx_ref_opt {
            exchange_actors.insert(Exchange::OKX, Box::new(r));
            tracing::info!(exchange = "OKX", "ExchangeActor ready");
        }
        if let Some(r) = hyper_ref_opt {
            exchange_actors.insert(Exchange::Hyperliquid, Box::new(r));
            tracing::info!(exchange = "Hyperliquid", "ExchangeActor ready");
        }
        if let Some(r) = ibkr_ref_opt {
            exchange_actors.insert(Exchange::IBKR, Box::new(r));
            tracing::info!(exchange = "IBKR", "ExchangeActor ready");
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
            ibkr_client: ibkr_client_ref,
            paper_pubsub,
            executors: Vec::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "ManagerActor stopped");
        Ok(())
    }

    /// 子 actor 终止时的处置：**区分"有意撤下"与"意外死亡"**。
    ///
    /// 这个区分是必需的：降级（[`RemoveStrategies`]）与投产失败回滚都会主动停掉 executor，
    /// 若一律判为"子 actor 挂了"就会整机退出 —— 撤下一个 symbol 的实例把整个引擎带走，
    /// 显然不是意图。
    ///
    /// 判据取自 kameo 的 stop reason，因此调用方**必须**用 `stop_gracefully()`（→ `Normal`）
    /// 表达"有意撤下"，而不是 `kill()`（→ `Killed`，与崩溃无法区分）。见
    /// [`Self::stop_executor`]。
    ///
    /// 其余情形（`Killed` / `Panicked` / 级联的 `LinkDied`）一律级联退出：本项目的错误处理
    /// 约定是"不重连、不降级运行"，一个子系统失效即受控停机，交由外部编排重启。
    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        if matches!(reason, ActorStopReason::Normal) {
            tracing::info!(actor_id = ?id, "子 actor 有序停止（有意撤下），引擎继续运行");
            return Ok(ControlFlow::Continue(()));
        }
        tracing::error!(actor_id = ?id, reason = ?reason, "Child actor died, shutting down");
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

/// 停止管理器
pub struct Stop;

impl Message<Stop> for ManagerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Stop, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        tracing::info!("Stopping ManagerActor...");
        ctx.actor_ref().stop_gracefully().await.ok();
    }
}

/// 订阅 Income 事件
pub struct SubscribeIncome<A: Actor>(pub ActorRef<A>);

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
        if let Err(e) = self.income_pubsub.tell(Subscribe(msg.0)).send().await {
            tracing::error!(error = %e, "Failed to publish to IncomePubSub");
        }
    }
}

/// 订阅 Outcome 事件
pub struct SubscribeOutcome<A: Actor>(pub ActorRef<A>);

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
        if let Err(e) = self.outcome_pubsub.tell(Subscribe(msg.0)).send().await {
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

/// 注入一条 income 事件到事件流 (供外部数据源如 SKHY 的 IBKR poller 把借券费/汇率推给策略)。
/// 事件经 income_pubsub 广播 → IncomeProcessor 按 (exchange,symbol) 路由到策略，
/// 与交易所 actor 产的行情事件走同一条流。
pub struct PublishIncome(pub IncomeEvent);

impl Message<PublishIncome> for ManagerActor {
    type Reply = ();

    async fn handle(&mut self, msg: PublishIncome, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        if let Err(e) = self
            .income_pubsub
            .tell(kameo_actors::pubsub::Publish(msg.0))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish injected income event");
        }
    }
}

/// 获取 IBKR 具体 client 引用 (供策略调用借券费/外汇等非 trait 方法)；未配置 IBKR 返回 None
pub struct GetIbkrClient;

impl Message<GetIbkrClient> for ManagerActor {
    type Reply = Option<Arc<IbkrClient>>;

    async fn handle(
        &mut self,
        _msg: GetIbkrClient,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.ibkr_client.clone()
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
            if let Err(reason) = Self::stop_executor(&reg.executor).await {
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
        if !is_live {
            return Ok(());
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
                entry_price: 0.0,
                unrealized_pnl: 0.0,
            };
            // 只平下到本次指定交易所的那些腿；空仓判据用 domain 的口径，不另写一份比较
            if exchange != msg.exchange || pos.is_empty() {
                continue;
            }
            let Some(meta) = self.symbol_metas.get(&(msg.exchange, pos.symbol.clone())) else {
                tracing::error!(
                    exchange = %msg.exchange,
                    symbol = %pos.symbol,
                    "No SymbolMeta — cannot flatten (数量无法折算为交易所单位)"
                );
                incomplete.push(format!("{}: 缺 SymbolMeta", pos.symbol));
                continue;
            };
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
            let wire = ExchangeOrder::from_domain(order, meta);
            if let Err(e) = client.place_order(wire).await {
                tracing::error!(
                    symbol = %pos.symbol,
                    error = %e,
                    "平仓下单失败 —— 实盘仍有敞口"
                );
                incomplete.push(format!("{}: 平仓下单失败 ({e})", pos.symbol));
            }
        }

        if !incomplete.is_empty() {
            // 实例已撤下但仓位没平干净：向上报错，让调用方保留"实盘仍开着"的记账状态。
            // 说成功会让 SupervisorActor 忘掉这个 symbol，敞口就此无人看管。
            return Err(ExchangeError::Other(format!(
                "降级未完成，{} 上仍可能有未平敞口，需人工核对: {}",
                msg.exchange,
                incomplete.join("; ")
            )));
        }
        Ok(())
    }
}

/// 订阅模拟账户私有事件总线（Supervisor / 观测层用）
pub struct SubscribePaper<A: Actor>(pub ActorRef<A>);

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
        if let Err(e) = self.paper_pubsub.tell(Subscribe(msg.0)).send().await {
            tracing::error!(error = %e, "Failed to subscribe to PaperPubSub");
        }
    }
}

#[cfg(test)]
mod leftover_order_tests {
    use super::*;
    use crate::domain::{AccountInfo, OrderId, OrderStatus, Position};
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
        async fn set_leverage(&self, _s: &Symbol, _l: u32) -> Result<(), ExchangeError> {
            unreachable!("撤单流程不该改杠杆")
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
            price: 100.0,
            reduce_only: false,
            quantity: 1.0,
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: 1,
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

    /// 本修复依赖 kameo 的一条行为：`stop_gracefully()` 让子 actor 以
    /// [`ActorStopReason::Normal`] 停止，而 `kill()` 以 `Killed` 停止 —— 父 actor 正是靠这个
    /// 区分"有意撤下"与"意外死亡"。若 kameo 改了这个语义，`ManagerActor::on_link_died` 会把
    /// 降级误判成崩溃、整机退出，而且**没有任何编译错误**。故用一对最小父子把它钉住。
    struct Child;

    impl Actor for Child {
        type Args = ();
        type Error = Infallible;
        async fn on_start(_: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
    }

    /// 与 `ManagerActor::on_link_died` 同样的判据：Normal 放行、其余级联退出
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
            if matches!(reason, ActorStopReason::Normal) {
                return Ok(ControlFlow::Continue(()));
            }
            Ok(ControlFlow::Break(ActorStopReason::LinkDied {
                id,
                reason: Box::new(reason),
            }))
        }
    }

    async fn run_and_observe(graceful: bool) -> (bool, Vec<String>) {
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

    /// **有意撤下**：子 actor 以 Normal 停止，父 actor 必须活着。
    ///
    /// 这条对应降级（RemoveStrategies）与投产失败回滚 —— 撤下一个 symbol 的实例不该把整个
    /// 引擎带走。
    #[tokio::test]
    async fn graceful_stop_reports_normal_and_parent_survives() {
        let (parent_alive, reasons) = run_and_observe(true).await;
        assert_eq!(
            reasons,
            vec!["Normal".to_string()],
            "stop_gracefully 不再产生 Normal —— on_link_died 的区分判据失效了"
        );
        assert!(parent_alive, "有意撤下子 actor 却把父 actor 带走了");
    }

    /// **意外死亡**：`kill()` 以 Killed 停止，父 actor 必须级联退出。
    ///
    /// 这条守住另一半：WS actor 解析失败时自 kill，必须能把整机带下去（本项目的约定是
    /// 不重连、不降级运行）。若两者都判为可继续，故障就会被静默吞掉。
    #[tokio::test]
    async fn killed_child_still_takes_the_parent_down() {
        let (parent_alive, reasons) = run_and_observe(false).await;
        assert_eq!(reasons, vec!["Killed".to_string()], "kill 的 reason 变了");
        assert!(!parent_alive, "子 actor 异常死亡却没有级联退出");
    }
}

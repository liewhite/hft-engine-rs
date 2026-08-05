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
    IncomeProcessorActor, MetricsActor, MetricsActorArgs, OutcomePubSub, OutcomeProcessorActor,
    RegisterExecutor, RegisterSymbols, OutcomeProcessorArgs, UnregisterExecutor,
    DEFAULT_REPORT_INTERVAL_MS,
};
use crate::domain::{
    now_ms, AccountId, Exchange, ExchangeError, Order, OrderType, Side, Symbol, SymbolMeta,
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

    /// 批量添加策略的内部实现
    async fn do_add_strategies(
        &mut self,
        strategies: Vec<StrategySpec>,
        actor_ref: ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        if strategies.is_empty() {
            return Ok(());
        }

        let processor = self.income_processor.clone();
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

        // 3. 批量创建 ExecutorActors
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

        // 4. 向 ProcessorActor 注册所有 Executor 的订阅
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
                tracing::error!(error = %e, "Failed to forward event to executor");
            }
            self.executors.push(RegisteredExecutor {
                executor: executor_ref.clone(),
                account: account.clone(),
                symbols: subscriptions.iter().map(|(_, k)| k.symbol().clone()).collect(),
            });
        }

        // 收集所有策略涉及的 (exchange, symbol) 对（下面 step 5 & 6 共用）
        let exchange_symbols: HashSet<(Exchange, Symbol)> = all_subscriptions
            .iter()
            .map(|(exchange, kind)| (*exchange, kind.symbol().clone()))
            .collect();

        // 4.5 告知观测层要跟踪哪些 symbol。**必须在推初始持仓之前**——否则观测层会因 symbol
        //     未注册而丢掉启动快照，进而丢掉盈亏基线。由 manager 自己从策略订阅推导并注册，
        //     调用方无需（也无从）关心这个时序。
        {
            let symbols: Vec<Symbol> = exchange_symbols
                .iter()
                .map(|(_, symbol)| symbol.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            if let Err(e) = self.metrics.ask(RegisterSymbols(symbols)).send().await {
                tracing::error!(error = %e, "Failed to register symbols on MetricsActor");
            }
        }

        // 5. 查询各交易所初始持仓，推到 income_pubsub。**必须在 executor 注册之后、
        //    市场订阅之前**进行——之前发布会被 BestEffort 丢，之后才发就有"开仓信号
        //    在持仓未对齐"的窗口。对每个 (exchange, symbol) 都推一条：交易所返回的
        //    用真实数据，未返回的推 size=0 显式清零（保证 SymbolState 一定收到初始值）。
        {
            // 仅实盘账户需要：这些事件走共享 IncomePubSub，只会路由给实盘账户的 executor；
            // 模拟账户从零起步，不需要也不应该看到真实持仓
            let exchanges: HashSet<Exchange> =
                exchange_symbols.iter().map(|(e, _)| *e).collect();
            for exchange in exchanges {
                let Some(client) = self.clients.get(&exchange) else {
                    continue;
                };
                let positions = match client.fetch_positions().await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            %exchange,
                            error = %e,
                            "Failed to fetch initial positions on startup, proceeding without"
                        );
                        continue;
                    }
                };
                let position_map: HashMap<Symbol, crate::domain::Position> =
                    positions.into_iter().map(|p| (p.symbol.clone(), p)).collect();
                let local_ts = now_ms();
                for (ex, symbol) in exchange_symbols.iter().filter(|(e, _)| *e == exchange) {
                    let pos = position_map.get(symbol).cloned().unwrap_or(crate::domain::Position {
                        exchange: *ex,
                        symbol: symbol.clone(),
                        size: 0.0,
                        entry_price: 0.0,
                        unrealized_pnl: 0.0,
                    });
                    tracing::info!(
                        %exchange,
                        %symbol,
                        size = pos.size,
                        "Initial position loaded"
                    );
                    let event = IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(pos),
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

        // 6. 查询各交易所现有挂单，作为 OrderUpdate 推送给 executor（在行情订阅之前）
        {
            for (exchange, symbol) in &exchange_symbols {
                let client = match self.clients.get(exchange) {
                    Some(c) => c,
                    None => continue,
                };
                match client.fetch_pending_orders(symbol).await {
                    Ok(updates) => {
                        if !updates.is_empty() {
                            tracing::info!(
                                %exchange,
                                %symbol,
                                count = updates.len(),
                                "Fetched existing pending orders on startup"
                            );
                        }
                        let local_ts = now_ms();
                        for update in updates {
                            // 数量无需在此折算：ExchangeClient 的契约是返回币本位
                            // (见 crate::domain::Quantity)，各所 client 内部已折算完毕
                            let event = IncomeEvent {
                                exchange_ts: local_ts,
                                local_ts,
                                data: ExchangeEventData::OrderUpdate(update),
                            };
                            // 通过 income_pubsub 广播，IncomeProcessor 会路由到对应 executor
                            if let Err(e) = self
                                .income_pubsub
                                .tell(kameo_actors::pubsub::Publish(event))
                                .send()
                                .await
                            {
                                tracing::error!(error = %e, "Failed to publish existing order update");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            %exchange,
                            %symbol,
                            error = %e,
                            "Failed to fetch pending orders on startup, proceeding without"
                        );
                    }
                }
            }
        }

        // 7. 批量向各 ExchangeActors 发送订阅请求（市场数据从此处开始流动）
        for (exchange, kinds) in exchange_subscriptions {
            if let Some(actor) = self.exchange_actors.get(&exchange) {
                actor
                    .subscribe_batch(kinds)
                    .await
                    .map_err(ExchangeError::Other)?;
            }
        }

        tracing::info!(
            count = executor_refs.len(),
            "Strategies batch added, ExecutorActors created"
        );

        Ok(())
    }
}

impl Actor for ManagerActor {
    type Args = ManagerActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 创建 Exchange Clients
        //    模拟盘也需要 client：symbol metas 走公共 REST，与是否有凭证无关
        let mut clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = HashMap::new();

        if let Some(ref access) = args.binance {
            let creds = access.credentials.clone();
            let client = BinanceClient::new(access.quote.clone(), creds)?;
            clients.insert(Exchange::Binance, Arc::new(client));
        }

        let okx_client: Option<Arc<OkxClient>> = if let Some(ref access) = args.okx {
            let creds = access.credentials.clone();
            let client = Arc::new(OkxClient::new(access.quote.clone(), creds)?);
            clients.insert(Exchange::OKX, client.clone());
            Some(client)
        } else {
            None
        };

        if let Some(ref access) = args.hyperliquid {
            let creds = access.credentials.clone();
            let client =
                HyperliquidClient::new(access.quote.clone(), access.dex.clone(), creds)?;
            clients.insert(Exchange::Hyperliquid, Arc::new(client));
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

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
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

        // 1. 先撤实例，再平仓。顺序不能反：先平仓的话，策略在收到平仓回报后可能立刻重开。
        let mut removed = 0usize;
        let mut kept = Vec::new();
        for reg in std::mem::take(&mut self.executors) {
            let hit = reg.account == msg.account
                && targets.iter().any(|s| reg.symbols.contains(s));
            if !hit {
                kept.push(reg);
                continue;
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
            reg.executor.kill();
            removed += 1;
        }
        self.executors = kept;
        tracing::info!(
            account = %msg.account,
            symbols = ?targets,
            removed,
            "Removed strategy instances"
        );

        if !msg.flatten {
            return Ok(());
        }

        // 2. 平仓只对实盘账户有意义：模拟账户的仓位在本地柜台里，随账户一起留存（供继续记账）
        if msg.account != AccountId::Live {
            return Ok(());
        }
        let Some(client) = self.clients.get(&msg.exchange).cloned() else {
            return Err(ExchangeError::Other(format!(
                "No client for {} — cannot flatten",
                msg.exchange
            )));
        };

        // 仓位以交易所为准：本地状态可能因撤下实例而不再更新
        let positions = client.fetch_positions().await?;
        for pos in positions {
            if !targets.contains(&pos.symbol) || pos.is_empty() {
                continue;
            }
            let Some(meta) = self.symbol_metas.get(&(msg.exchange, pos.symbol.clone())) else {
                tracing::error!(
                    exchange = %msg.exchange,
                    symbol = %pos.symbol,
                    "No SymbolMeta — cannot flatten (数量无法折算为交易所单位)"
                );
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
                    "平仓下单失败 —— 实盘仍有敞口，需人工介入"
                );
            }
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

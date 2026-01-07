//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 创建 PubSub Actors (IncomePubSub, OutcomePubSub)
//! - 使用 spawn + link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use super::{
    ClockActor, ClockActorArgs, ExecutorActor, ExecutorArgs, IncomePubSub,
    IncomeProcessorActor, OutcomePubSub, OutcomeProcessorActor, RegisterExecutor,
    SignalProcessorArgs,
};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{
    BinanceActor, BinanceActorArgs, BinanceCredentials, BinanceModule, REST_BASE_URL,
};
use crate::exchange::hyperliquid::{
    HyperliquidActor, HyperliquidActorArgs, HyperliquidCredentials, HyperliquidModule,
};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxCredentials, OkxModule};
use crate::exchange::{ExchangeActorOps, ExchangeClient, ExchangeModule, SubscriptionKind};
use crate::strategy::Strategy;
use kameo::actor::{ActorId, ActorRef, PreparedActor, Spawn, WeakActorRef};
use std::ops::ControlFlow;
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Subscribe;
use kameo_actors::DeliveryStrategy;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ============================================================================
// 子 Actor 类型标识
// ============================================================================

/// 子 Actor 类型（用于 on_link_died 中识别）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildActorKind {
    Exchange(Exchange),
    Executor(usize),
    SignalProcessor,
    Processor,
    Clock,
    IncomePubSub,
    OutcomePubSub,
}

// ============================================================================
// ManagerActor
// ============================================================================

/// ManagerActor 初始化参数
pub struct ManagerActorArgs {
    /// Binance 凭证（可选）
    pub binance_credentials: Option<BinanceCredentials>,
    /// OKX 凭证（可选）
    pub okx_credentials: Option<OkxCredentials>,
    /// Hyperliquid 凭证（可选）
    pub hyperliquid_credentials: Option<HyperliquidCredentials>,
}

/// ManagerActor - 顶层管理 Actor
pub struct ManagerActor {
    // === Exchange Modules ===
    modules: HashMap<Exchange, Arc<dyn ExchangeModule>>,

    // === Credentials ===
    binance_credentials: Option<BinanceCredentials>,
    okx_credentials: Option<OkxCredentials>,
    hyperliquid_credentials: Option<HyperliquidCredentials>,

    // === Symbol Metas 缓存 ===
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,

    // === PubSub Actors ===
    /// Income PubSub (行情/账户事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Outcome PubSub (策略信号)
    outcome_pubsub: ActorRef<OutcomePubSub>,

    // === 子 Actors ===
    /// ProcessorActor (订阅 income_pubsub)
    processor: ActorRef<IncomeProcessorActor>,
    /// SignalProcessorActor (订阅 outcome_pubsub) - 保持引用以维持生命周期
    _signal_processor: ActorRef<OutcomeProcessorActor>,
    /// ClockActor - 保持引用以维持生命周期
    _clock_actor: ActorRef<ClockActor>,
    /// ExchangeActors (惰性创建，类型擦除)
    exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>>,
    /// ExecutorActors (add_strategy 时创建)
    executors: Vec<ActorRef<ExecutorActor>>,

    /// ActorId -> ChildActorKind 映射（用于 on_link_died）
    actor_kinds: HashMap<ActorId, ChildActorKind>,
}

impl ManagerActor {
    /// 创建并启动 ManagerActor，返回 ActorRef
    ///
    /// 在返回之前完成所有初始化工作：
    /// - 创建 Exchange Modules
    /// - 预加载所有交易所的 symbol metas
    /// - 创建 PubSub Actors
    /// - 创建并 link 所有子 Actors
    pub async fn new(args: ManagerActorArgs) -> ActorRef<Self> {
        // 1. 创建 Exchange Modules
        let mut modules: HashMap<Exchange, Arc<dyn ExchangeModule>> = HashMap::new();

        if let Some(ref cred) = args.binance_credentials {
            let module =
                BinanceModule::new(Some(cred.clone())).expect("Failed to create BinanceModule");
            modules.insert(Exchange::Binance, Arc::new(module));
        }

        if let Some(ref cred) = args.okx_credentials {
            let module = OkxModule::new(Some(cred.clone())).expect("Failed to create OkxModule");
            modules.insert(Exchange::OKX, Arc::new(module));
        }

        if let Some(ref cred) = args.hyperliquid_credentials {
            let module = HyperliquidModule::new(Some(cred.clone()))
                .expect("Failed to create HyperliquidModule");
            modules.insert(Exchange::Hyperliquid, Arc::new(module));
        }

        // 2. 预加载所有交易所的 symbol metas
        let symbol_metas = Self::preload_all_symbol_metas(&modules)
            .await
            .expect("Failed to preload symbol metas");

        // 3. 创建 PubSub Actors (使用 unbounded mailbox)
        let income_pubsub = IncomePubSub::spawn_with_mailbox(
            IncomePubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        );
        let outcome_pubsub = OutcomePubSub::spawn_with_mailbox(
            OutcomePubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        );

        // 4. 创建 ProcessorActor 并订阅 income_pubsub
        let processor = IncomeProcessorActor::spawn_with_mailbox(
            IncomeProcessorActor::default(),
            mailbox::unbounded(),
        );
        let _ = income_pubsub
            .tell(Subscribe(processor.clone()))
            .send()
            .await;

        // 5. 获取 configured_clients (用于 SignalProcessorActor)
        let configured_clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = modules
            .iter()
            .map(|(e, m)| (*e, m.client()))
            .collect();

        // 6. 创建 SignalProcessorActor 并订阅 outcome_pubsub
        let signal_processor = OutcomeProcessorActor::spawn_with_mailbox(
            SignalProcessorArgs {
                clients: configured_clients,
                income_pubsub: income_pubsub.clone(),
            },
            mailbox::unbounded(),
        );
        let _ = outcome_pubsub
            .tell(Subscribe(signal_processor.clone()))
            .send()
            .await;

        // 7. 创建 ClockActor（发布 Clock 事件到 income_pubsub）
        let clock_actor = ClockActor::spawn_with_mailbox(
            ClockActorArgs {
                interval_ms: 1000,
                income_pubsub: income_pubsub.clone(),
            },
            mailbox::unbounded(),
        );

        // 8. 构建 actor_kinds 映射
        let mut actor_kinds = HashMap::new();
        actor_kinds.insert(income_pubsub.id(), ChildActorKind::IncomePubSub);
        actor_kinds.insert(outcome_pubsub.id(), ChildActorKind::OutcomePubSub);
        actor_kinds.insert(processor.id(), ChildActorKind::Processor);
        actor_kinds.insert(signal_processor.id(), ChildActorKind::SignalProcessor);
        actor_kinds.insert(clock_actor.id(), ChildActorKind::Clock);

        // 9. 构建 ManagerActor state
        let state = Self {
            modules,
            binance_credentials: args.binance_credentials,
            okx_credentials: args.okx_credentials,
            hyperliquid_credentials: args.hyperliquid_credentials,
            symbol_metas,
            income_pubsub: income_pubsub.clone(),
            outcome_pubsub: outcome_pubsub.clone(),
            processor: processor.clone(),
            _signal_processor: signal_processor.clone(),
            _clock_actor: clock_actor.clone(),
            exchange_actors: HashMap::new(),
            executors: Vec::new(),
            actor_kinds,
        };

        // 10. 使用 PreparedActor 准备 ManagerActor 并获取 actor_ref
        let prepared = PreparedActor::new(mailbox::unbounded());
        let manager_ref = prepared.actor_ref().clone();

        // 11. 建立双向 link (在 spawn 之前)
        manager_ref.link(&income_pubsub).await;
        manager_ref.link(&outcome_pubsub).await;
        manager_ref.link(&processor).await;
        manager_ref.link(&signal_processor).await;
        manager_ref.link(&clock_actor).await;

        // 12. spawn ManagerActor
        prepared.spawn(state);

        tracing::info!("ManagerActor created with all child actors linked");
        manager_ref
    }

    /// 预加载所有交易所的 symbol metas（静态方法）
    async fn preload_all_symbol_metas(
        modules: &HashMap<Exchange, Arc<dyn ExchangeModule>>,
    ) -> Result<HashMap<(Exchange, Symbol), SymbolMeta>, ExchangeError> {
        let mut symbol_metas = HashMap::new();

        for (exchange, module) in modules {
            let client = module.client();
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                symbol_metas.insert((*exchange, meta.symbol.clone()), meta);
            }
            tracing::info!(%exchange, count, "Preloaded symbol metas");
        }

        Ok(symbol_metas)
    }

    /// 检查策略所需的 symbols 是否都已缓存
    fn check_required_symbols(&self, strategy: &dyn Strategy) -> Result<(), ExchangeError> {
        let public_streams = strategy.public_streams();
        for (exchange, kinds) in &public_streams {
            for kind in kinds {
                let symbol = kind.symbol();
                if !self.symbol_metas.contains_key(&(*exchange, symbol.clone())) {
                    return Err(ExchangeError::Other(format!(
                        "Symbol {:?} not found on {:?}. May be newly listed, please restart.",
                        symbol, exchange
                    )));
                }
            }
        }
        Ok(())
    }

    /// 提取指定交易所的 symbol metas
    fn get_symbol_metas_for(&self, exchange: Exchange) -> Arc<HashMap<Symbol, SymbolMeta>> {
        Arc::new(
            self.symbol_metas
                .iter()
                .filter(|((e, _), _)| *e == exchange)
                .map(|((_, s), m)| (s.clone(), m.clone()))
                .collect(),
        )
    }

    /// 惰性创建 ExchangeActor（如不存在则 spawn + link）
    async fn ensure_exchange_actor(
        &mut self,
        exchange: Exchange,
        actor_ref: &ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        if self.exchange_actors.contains_key(&exchange) {
            return Ok(());
        }

        let income_pubsub = self.income_pubsub.clone();
        let symbol_metas = self.get_symbol_metas_for(exchange);

        // 根据交易所类型创建对应的 Actor
        let (actor_id, boxed_actor): (ActorId, Box<dyn ExchangeActorOps>) = match exchange {
            Exchange::Binance => {
                let credentials = self
                    .binance_credentials
                    .clone()
                    .expect("Binance credentials not configured");
                let client = self
                    .modules
                    .get(&Exchange::Binance)
                    .expect("Binance module not configured")
                    .client();
                let binance_ref = BinanceActor::new(BinanceActorArgs {
                    credentials: Some(credentials),
                    symbol_metas: symbol_metas.clone(),
                    rest_base_url: REST_BASE_URL.to_string(),
                    income_pubsub: income_pubsub.clone(),
                    client,
                })
                .await;
                actor_ref.link(&binance_ref).await;
                (binance_ref.id(), Box::new(binance_ref))
            }
            Exchange::OKX => {
                let credentials = self
                    .okx_credentials
                    .clone()
                    .expect("OKX credentials not configured");
                let okx_ref = OkxActor::new(OkxActorArgs {
                    credentials: Some(credentials),
                    symbol_metas: symbol_metas.clone(),
                    income_pubsub: income_pubsub.clone(),
                })
                .await;
                actor_ref.link(&okx_ref).await;
                (okx_ref.id(), Box::new(okx_ref))
            }
            Exchange::Hyperliquid => {
                let credentials = self
                    .hyperliquid_credentials
                    .clone()
                    .expect("Hyperliquid credentials not configured");
                let hyper_ref = HyperliquidActor::new(HyperliquidActorArgs {
                    credentials: Some(credentials),
                    symbol_metas: symbol_metas.clone(),
                    income_pubsub: income_pubsub.clone(),
                })
                .await;
                actor_ref.link(&hyper_ref).await;
                (hyper_ref.id(), Box::new(hyper_ref))
            }
        };

        self.actor_kinds
            .insert(actor_id, ChildActorKind::Exchange(exchange));
        self.exchange_actors.insert(exchange, boxed_actor);

        tracing::info!(%exchange, "ExchangeActor created (lazy)");
        Ok(())
    }

    /// 添加策略的内部实现
    async fn do_add_strategy(
        &mut self,
        strategy: Box<dyn Strategy>,
        actor_ref: ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        let processor = self.processor.clone();
        let outcome_pubsub = self.outcome_pubsub.clone();

        // 1. 检查策略所需的 symbols 是否都已缓存（启动时已预加载）
        self.check_required_symbols(strategy.as_ref())?;

        // 2. 获取策略需要的 public streams
        let public_streams = strategy.public_streams();

        // 3. 惰性创建所需的 ExchangeActors
        for exchange in public_streams.keys() {
            self.ensure_exchange_actor(*exchange, &actor_ref).await?;
        }

        // 4. 收集策略需要的订阅
        let subscriptions: HashSet<(Exchange, SubscriptionKind)> = public_streams
            .iter()
            .flat_map(|(exchange, kinds)| kinds.iter().map(move |kind| (*exchange, kind.clone())))
            .collect();

        // 5. 创建 ExecutorActor
        let executor_idx = self.executors.len();

        // Prepare executor with link
        let prepared = PreparedActor::<ExecutorActor>::new(mailbox::unbounded());
        let executor_ref = prepared.actor_ref().clone();
        actor_ref.link(&executor_ref).await;
        prepared.spawn(ExecutorArgs {
            strategy,
            symbol_metas: Arc::new(self.symbol_metas.clone()),
            outcome_pubsub,
        });

        self.actor_kinds
            .insert(executor_ref.id(), ChildActorKind::Executor(executor_idx));

        // 6. 向 ProcessorActor 注册 Executor 的订阅
        let _ = processor
            .tell(RegisterExecutor {
                executor: executor_ref.clone(),
                subscriptions: subscriptions.clone(),
            })
            .send()
            .await;

        self.executors.push(executor_ref);

        // 7. 向 ExchangeActors 发送订阅请求
        for (exchange, kind) in subscriptions {
            if let Some(actor) = self.exchange_actors.get(&exchange) {
                actor
                    .subscribe(kind)
                    .await
                    .expect("Failed to subscribe to ExchangeActor");
            }
        }

        tracing::info!(
            executor_idx = executor_idx,
            "Strategy added, ExecutorActor created"
        );

        Ok(())
    }
}

impl Actor for ManagerActor {
    type Args = Self;
    type Error = Infallible;

    async fn on_start(state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 所有初始化工作已在 new() 中完成
        tracing::info!("ManagerActor started");
        Ok(state)
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
        let kind = self.actor_kinds.remove(&id);

        match kind {
            Some(ChildActorKind::Exchange(exchange)) => {
                tracing::error!(
                    %exchange,
                    reason = ?reason,
                    "ExchangeActor died, shutting down"
                );
                self.exchange_actors.remove(&exchange);
            }
            Some(ChildActorKind::Executor(idx)) => {
                tracing::error!(
                    executor_idx = idx,
                    reason = ?reason,
                    "ExecutorActor died, shutting down"
                );
            }
            Some(ChildActorKind::SignalProcessor) => {
                tracing::error!(reason = ?reason, "SignalProcessorActor died, shutting down");
            }
            Some(ChildActorKind::Processor) => {
                tracing::error!(reason = ?reason, "ProcessorActor died, shutting down");
            }
            Some(ChildActorKind::Clock) => {
                tracing::error!(reason = ?reason, "ClockActor died, shutting down");
            }
            Some(ChildActorKind::IncomePubSub) => {
                tracing::error!(reason = ?reason, "IncomePubSub died, shutting down");
            }
            Some(ChildActorKind::OutcomePubSub) => {
                tracing::error!(reason = ?reason, "OutcomePubSub died, shutting down");
            }
            None => {
                tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
                return Ok(ControlFlow::Continue(()));
            }
        }

        // 任何已知子 actor 死亡都级联退出
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
pub struct AddStrategy(pub Box<dyn Strategy>);

impl Message<AddStrategy> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: AddStrategy,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.do_add_strategy(msg.0, ctx.actor_ref().clone()).await
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

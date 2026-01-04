//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 使用 spawn + link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use super::{
    ClockActor, ClockArgs, ExecutorActor, ExecutorArgs, ProcessorActor, RegisterClockExecutor,
    RegisterExecutor, SignalProcessorActor,
};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{
    BinanceActor, BinanceActorArgs, BinanceCredentials, BinanceModule, REST_BASE_URL,
};
use crate::exchange::hyperliquid::{
    HyperliquidActor, HyperliquidActorArgs, HyperliquidCredentials, HyperliquidModule,
};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxCredentials, OkxModule};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient, ExchangeModule, SubscriptionKind};
use crate::messaging::IncomeEvent;
use crate::strategy::Strategy;
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef, PreparedActor, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
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

    // === 子 Actors (在 new() 中创建) ===
    /// SignalProcessorActor
    signal_processor: ActorRef<SignalProcessorActor>,
    /// ProcessorActor
    processor: ActorRef<ProcessorActor>,
    /// ClockActor
    clock_actor: ActorRef<ClockActor<ProcessorEventSink>>,
    /// ExchangeActors (惰性创建，类型擦除)
    exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>>,
    /// ExecutorActors (add_strategy 时创建)
    executors: Vec<ActorRef<ExecutorActor>>,

    /// ActorID -> ChildActorKind 映射（用于 on_link_died）
    actor_kinds: HashMap<ActorID, ChildActorKind>,

    /// ProcessorEventSink（供 ExchangeActor 使用）
    event_sink: Arc<dyn EventSink>,
}

impl ManagerActor {
    /// 创建并启动 ManagerActor，返回 ActorRef
    ///
    /// 在返回之前完成所有初始化工作：
    /// - 创建 Exchange Modules
    /// - 预加载所有交易所的 symbol metas
    /// - 创建并 link 所有子 Actors (Processor, SignalProcessor, Clock)
    pub async fn new(args: ManagerActorArgs) -> ActorRef<Self> {
        // 1. 创建 Exchange Modules
        let mut modules: HashMap<Exchange, Arc<dyn ExchangeModule>> = HashMap::new();

        if let Some(ref cred) = args.binance_credentials {
            let module = BinanceModule::new(Some(cred.clone()))
                .expect("Failed to create BinanceModule");
            modules.insert(Exchange::Binance, Arc::new(module));
        }

        if let Some(ref cred) = args.okx_credentials {
            let module = OkxModule::new(Some(cred.clone()))
                .expect("Failed to create OkxModule");
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

        // 3. 创建 ProcessorActor
        let processor = kameo::spawn(ProcessorActor::new());

        // 4. 创建 ProcessorEventSink
        let event_sink: Arc<dyn EventSink> = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });

        // 5. 获取 configured_clients (用于 SignalProcessorActor)
        let configured_clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = modules
            .iter()
            .map(|(e, m)| (*e, m.client()))
            .collect();

        // 6. 创建 SignalProcessorActor
        let signal_processor = kameo::spawn(SignalProcessorActor::new(super::SignalProcessorArgs {
            clients: configured_clients,
            event_sink: event_sink.clone(),
        }));

        // 7. 创建 ClockActor
        let clock_event_sink = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });
        let binance_client = modules.get(&Exchange::Binance).map(|m| m.client());
        let clock_actor = kameo::spawn(ClockActor::new(ClockArgs {
            interval_ms: 1000,
            binance_client,
            event_sink: clock_event_sink,
        }));

        // 8. 构建 actor_kinds 映射
        let mut actor_kinds = HashMap::new();
        actor_kinds.insert(processor.id(), ChildActorKind::Processor);
        actor_kinds.insert(signal_processor.id(), ChildActorKind::SignalProcessor);
        actor_kinds.insert(clock_actor.id(), ChildActorKind::Clock);

        // 9. 构建 ManagerActor
        let manager = Self {
            modules,
            binance_credentials: args.binance_credentials,
            okx_credentials: args.okx_credentials,
            hyperliquid_credentials: args.hyperliquid_credentials,
            symbol_metas,
            signal_processor: signal_processor.clone(),
            processor: processor.clone(),
            clock_actor: clock_actor.clone(),
            exchange_actors: HashMap::new(),
            executors: Vec::new(),
            actor_kinds,
            event_sink,
        };

        // 10. 使用 PreparedActor 准备 ManagerActor 并获取 actor_ref
        let prepared = PreparedActor::<Self>::new();
        let manager_ref = prepared.actor_ref().clone();

        // 11. 建立双向 link (在 spawn 之前)
        manager_ref.link(&processor).await;
        manager_ref.link(&signal_processor).await;
        manager_ref.link(&clock_actor).await;

        // 12. spawn ManagerActor
        prepared.spawn(manager);

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

    /// 获取所有已配置的 ExchangeClient (用于遍历)
    fn configured_clients(&self) -> HashMap<Exchange, Arc<dyn ExchangeClient>> {
        self.modules
            .iter()
            .map(|(e, m)| (*e, m.client()))
            .collect()
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

    /// 惰性创建 ExchangeActor（如不存在则 spawn_link）
    ///
    /// # Panics
    /// 如果对应交易所的 credentials 未配置则 panic
    async fn ensure_exchange_actor(
        &mut self,
        exchange: Exchange,
        actor_ref: &ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        if self.exchange_actors.contains_key(&exchange) {
            return Ok(());
        }

        let event_sink = self.event_sink.clone();
        let symbol_metas = self.get_symbol_metas_for(exchange);

        // 根据交易所类型创建对应的 Actor（直接使用 credentials，未配置则 panic）
        let (actor_id, boxed_actor): (ActorID, Box<dyn ExchangeActorOps>) = match exchange {
            Exchange::Binance => {
                let credentials = self
                    .binance_credentials
                    .clone()
                    .expect("Binance credentials not configured");
                let actor = BinanceActor::new(BinanceActorArgs {
                    credentials: Some(credentials),
                    symbol_metas: symbol_metas.clone(),
                    rest_base_url: REST_BASE_URL.to_string(),
                    event_sink: event_sink.clone(),
                });
                let actor_ref = spawn_link(actor_ref, actor).await;
                (actor_ref.id(), Box::new(actor_ref))
            }
            Exchange::OKX => {
                let credentials = self
                    .okx_credentials
                    .clone()
                    .expect("OKX credentials not configured");
                let actor = OkxActor::new(OkxActorArgs {
                    credentials: Some(credentials),
                    symbol_metas: symbol_metas.clone(),
                    event_sink: event_sink.clone(),
                });
                let actor_ref = spawn_link(actor_ref, actor).await;
                (actor_ref.id(), Box::new(actor_ref))
            }
            Exchange::Hyperliquid => {
                let credentials = self
                    .hyperliquid_credentials
                    .clone()
                    .expect("Hyperliquid credentials not configured");
                let actor = HyperliquidActor::new(HyperliquidActorArgs {
                    credentials: Some(credentials),
                    symbol_metas: symbol_metas.clone(),
                    event_sink: event_sink.clone(),
                });
                let actor_ref = spawn_link(actor_ref, actor).await;
                (actor_ref.id(), Box::new(actor_ref))
            }
        };

        self.actor_kinds.insert(actor_id, ChildActorKind::Exchange(exchange));
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
        let signal_processor = self.signal_processor.clone();

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

        let executor = ExecutorActor::new(ExecutorArgs {
            strategy,
            symbol_metas: Arc::new(self.symbol_metas.clone()),
            signal_processor: signal_processor.clone(),
        });

        let executor_ref = spawn_link(&actor_ref, executor).await;
        self.actor_kinds
            .insert(executor_ref.id(), ChildActorKind::Executor(executor_idx));

        // 6. 向 ProcessorActor 注册 Executor 的订阅
        processor
            .tell(RegisterExecutor {
                executor: executor_ref.clone(),
                subscriptions: subscriptions.clone(),
            })
            .await
            .expect("Failed to register executor to ProcessorActor");

        // 7. 向 ClockActor 注册 Executor（用于接收 Clock 事件）
        self.clock_actor
            .tell(RegisterClockExecutor {
                executor: executor_ref.clone(),
            })
            .await
            .expect("Failed to register executor to ClockActor");

        self.executors.push(executor_ref);

        // 8. 向 ExchangeActors 发送订阅请求
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
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ManagerActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 所有初始化工作已在 new() 中完成
        tracing::info!("ManagerActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!(reason = ?reason, "ManagerActor stopped");
        Ok(())
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorID,
        reason: ActorStopReason,
    ) -> Result<Option<ActorStopReason>, BoxError> {
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
            None => {
                tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
                return Ok(None);
            }
        }

        // 任何已知子 actor 死亡都级联退出
        Ok(Some(ActorStopReason::LinkDied {
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
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.do_add_strategy(msg.0, ctx.actor_ref().clone()).await
    }
}

/// 停止管理器
pub struct Stop;

impl Message<Stop> for ManagerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Stop, ctx: Context<'_, Self, Self::Reply>) {
        tracing::info!("Stopping ManagerActor...");
        ctx.actor_ref().stop_gracefully().await.ok();
    }
}

// ============================================================================
// Sink 实现
// ============================================================================

/// ProcessorActor 的事件接收器
pub struct ProcessorEventSink {
    processor: ActorRef<ProcessorActor>,
}

#[async_trait]
impl EventSink for ProcessorEventSink {
    async fn send_event(&self, event: IncomeEvent) {
        self.processor
            .tell(event)
            .await
            .expect("Failed to send event to ProcessorActor");
    }
}

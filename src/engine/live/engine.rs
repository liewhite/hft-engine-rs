//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 使用 spawn_link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use super::{
    ClockActor, ClockArgs, ExecutorActor, ExecutorArgs, ProcessorActor, RegisterClockExecutor,
    RegisterExecutor, SignalProcessorActor,
};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{BinanceActor, BinanceActorArgs, REST_BASE_URL};
use crate::exchange::hyperliquid::{HyperliquidActor, HyperliquidActorArgs};
use crate::exchange::okx::{OkxActor, OkxActorArgs};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient, ExchangeModule, SubscriptionKind};
use crate::messaging::IncomeEvent;
use crate::strategy::Strategy;
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
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
    /// 交易所模块列表（用于 REST 客户端访问和创建 Actor）
    pub modules: Vec<Arc<dyn ExchangeModule>>,
}

/// ManagerActor - 顶层管理 Actor
pub struct ManagerActor {
    // === Exchange Modules ===
    modules: HashMap<Exchange, Arc<dyn ExchangeModule>>,

    // === Symbol Metas 缓存 ===
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,

    // === 子 Actors ===
    /// SignalProcessorActor (on_start 创建)
    signal_processor: Option<ActorRef<SignalProcessorActor>>,
    /// ProcessorActor (on_start 创建)
    processor: Option<ActorRef<ProcessorActor>>,
    /// ClockActor (on_start 创建)
    clock_actor: Option<ActorRef<ClockActor<ProcessorEventSink>>>,
    /// ExchangeActors (惰性创建，类型擦除)
    exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>>,
    /// ExecutorActors (add_strategy 时创建)
    executors: Vec<ActorRef<ExecutorActor>>,

    /// ActorID -> ChildActorKind 映射（用于 on_link_died）
    actor_kinds: HashMap<ActorID, ChildActorKind>,

    /// ProcessorEventSink（on_start 后设置，供 ExchangeActor 使用）
    event_sink: Option<Arc<dyn EventSink>>,
}

impl ManagerActor {
    pub fn new(args: ManagerActorArgs) -> Self {
        let modules = args
            .modules
            .into_iter()
            .map(|m| (m.exchange(), m))
            .collect();

        Self {
            modules,
            symbol_metas: HashMap::new(),
            signal_processor: None,
            processor: None,
            clock_actor: None,
            exchange_actors: HashMap::new(),
            executors: Vec::new(),
            actor_kinds: HashMap::new(),
            event_sink: None,
        }
    }

    /// 获取所有已配置的 ExchangeClient (用于遍历)
    fn configured_clients(&self) -> HashMap<Exchange, Arc<dyn ExchangeClient>> {
        self.modules
            .iter()
            .map(|(e, m)| (*e, m.client()))
            .collect()
    }

    /// 预加载所有交易所的 symbol metas
    async fn preload_symbol_metas(&mut self) -> Result<(), ExchangeError> {
        for (exchange, client) in self.configured_clients() {
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                self.symbol_metas.insert((exchange, meta.symbol.clone()), meta);
            }
            tracing::info!(%exchange, count, "Preloaded symbol metas");
        }
        Ok(())
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
    async fn ensure_exchange_actor(
        &mut self,
        exchange: Exchange,
        actor_ref: &ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        if self.exchange_actors.contains_key(&exchange) {
            return Ok(());
        }

        let event_sink = self.event_sink.clone().ok_or_else(|| {
            ExchangeError::Other("EventSink not initialized".to_string())
        })?;

        let symbol_metas = self.get_symbol_metas_for(exchange);
        let module = self.modules.get(&exchange).ok_or_else(|| {
            ExchangeError::Other(format!("{} module not configured", exchange))
        })?;

        // 根据交易所类型创建对应的 Actor (通过 downcast 获取 credentials)
        let (actor_id, boxed_actor): (ActorID, Box<dyn ExchangeActorOps>) = match exchange {
            Exchange::Binance => {
                let actor = BinanceActor::new(BinanceActorArgs {
                    credentials: module
                        .client()
                        .as_any()
                        .downcast_ref::<crate::exchange::binance::BinanceClient>()
                        .and_then(|c| c.credentials().cloned()),
                    symbol_metas: symbol_metas.clone(),
                    rest_base_url: REST_BASE_URL.to_string(),
                    event_sink: event_sink.clone(),
                });
                let actor_ref = spawn_link(actor_ref, actor).await;
                (actor_ref.id(), Box::new(actor_ref))
            }
            Exchange::OKX => {
                let actor = OkxActor::new(OkxActorArgs {
                    credentials: module.client()
                        .as_any()
                        .downcast_ref::<crate::exchange::okx::OkxClient>()
                        .and_then(|c| c.credentials().cloned()),
                    symbol_metas: symbol_metas.clone(),
                    event_sink: event_sink.clone(),
                });
                let actor_ref = spawn_link(actor_ref, actor).await;
                (actor_ref.id(), Box::new(actor_ref))
            }
            Exchange::Hyperliquid => {
                let actor = HyperliquidActor::new(HyperliquidActorArgs {
                    credentials: module.client()
                        .as_any()
                        .downcast_ref::<crate::exchange::hyperliquid::HyperliquidClient>()
                        .and_then(|c| c.credentials().cloned()),
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
        let processor = self.processor.clone().ok_or_else(|| {
            ExchangeError::Other("ProcessorActor not initialized".to_string())
        })?;

        let signal_processor = self.signal_processor.clone().ok_or_else(|| {
            ExchangeError::Other("SignalProcessorActor not initialized".to_string())
        })?;

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
        if let Some(clock_actor) = &self.clock_actor {
            clock_actor
                .tell(RegisterClockExecutor {
                    executor: executor_ref.clone(),
                })
                .await
                .expect("Failed to register executor to ClockActor");
        }

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

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 1. 预加载所有交易所的 symbol metas
        self.preload_symbol_metas().await?;

        // 2. 创建 ProcessorActor
        let processor = spawn_link(&actor_ref, ProcessorActor::new()).await;
        self.actor_kinds
            .insert(processor.id(), ChildActorKind::Processor);
        self.processor = Some(processor.clone());

        // 3. 创建 ProcessorEventSink（供 ExchangeActors 使用）
        let event_sink: Arc<dyn EventSink> = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });
        self.event_sink = Some(event_sink.clone());

        // 4. 创建 SignalProcessorActor
        let signal_processor = spawn_link(
            &actor_ref,
            SignalProcessorActor::new(super::SignalProcessorArgs {
                clients: self.configured_clients(),
                event_sink: event_sink.clone(),
            }),
        )
        .await;
        self.actor_kinds
            .insert(signal_processor.id(), ChildActorKind::SignalProcessor);
        self.signal_processor = Some(signal_processor);

        // 5. 创建 ClockActor
        let clock_event_sink = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });
        let binance_client = self
            .modules
            .get(&Exchange::Binance)
            .map(|m| m.client());
        let clock_actor = spawn_link(
            &actor_ref,
            ClockActor::new(ClockArgs {
                interval_ms: 1000,
                binance_client,
                event_sink: clock_event_sink,
            }),
        )
        .await;
        self.actor_kinds
            .insert(clock_actor.id(), ChildActorKind::Clock);
        self.clock_actor = Some(clock_actor);

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
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::Executor(idx)) => {
                tracing::error!(
                    executor_idx = idx,
                    reason = ?reason,
                    "ExecutorActor died, shutting down"
                );
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::SignalProcessor) => {
                tracing::error!(reason = ?reason, "SignalProcessorActor died, shutting down");
                self.signal_processor = None;
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::Processor) => {
                tracing::error!(reason = ?reason, "ProcessorActor died, shutting down");
                self.processor = None;
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::Clock) => {
                tracing::error!(reason = ?reason, "ClockActor died, shutting down");
                self.clock_actor = None;
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            None => {
                tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
                Ok(None)
            }
        }
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

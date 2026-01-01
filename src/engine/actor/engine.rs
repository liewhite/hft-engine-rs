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
use crate::exchange::binance::{BinanceActor, BinanceActorArgs, BinanceClient};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxClient};
use crate::exchange::{EventSink, ExchangeClient, Subscribe, SubscriptionKind};
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
    /// Binance 客户端（可选）
    pub binance_client: Option<Arc<BinanceClient>>,
    /// OKX 客户端（可选）
    pub okx_client: Option<Arc<OkxClient>>,
}

/// ManagerActor - 顶层管理 Actor
pub struct ManagerActor {
    // === Exchange Clients (REST) ===
    binance_client: Option<Arc<BinanceClient>>,
    okx_client: Option<Arc<OkxClient>>,

    // === Symbol Metas 缓存 ===
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,

    // === 子 Actors ===
    /// SignalProcessorActor (on_start 创建)
    signal_processor: Option<ActorRef<SignalProcessorActor>>,
    /// ProcessorActor (on_start 创建)
    processor: Option<ActorRef<ProcessorActor>>,
    /// ClockActor (on_start 创建)
    clock_actor: Option<ActorRef<ClockActor<ProcessorEventSink>>>,
    /// BinanceActor (按需创建)
    binance_actor: Option<ActorRef<BinanceActor>>,
    /// OkxActor (按需创建)
    okx_actor: Option<ActorRef<OkxActor>>,
    /// ExecutorActors (add_strategy 时创建)
    executors: Vec<ActorRef<ExecutorActor>>,

    /// ActorID -> ChildActorKind 映射（用于 on_link_died）
    actor_kinds: HashMap<ActorID, ChildActorKind>,

    /// Actor 自身引用
    self_ref: Option<ActorRef<Self>>,
}

impl ManagerActor {
    pub fn new(args: ManagerActorArgs) -> Self {
        Self {
            binance_client: args.binance_client,
            okx_client: args.okx_client,
            symbol_metas: HashMap::new(),
            signal_processor: None,
            processor: None,
            clock_actor: None,
            binance_actor: None,
            okx_actor: None,
            executors: Vec::new(),
            actor_kinds: HashMap::new(),
            self_ref: None,
        }
    }

    /// 预加载所有交易所的 symbol metas
    async fn preload_symbol_metas(&mut self) -> Result<(), ExchangeError> {
        // Binance
        if let Some(client) = &self.binance_client {
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                self.symbol_metas.insert((Exchange::Binance, meta.symbol.clone()), meta);
            }
            tracing::info!(exchange = %Exchange::Binance, count, "Preloaded symbol metas");
        }

        // OKX
        if let Some(client) = &self.okx_client {
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                self.symbol_metas.insert((Exchange::OKX, meta.symbol.clone()), meta);
            }
            tracing::info!(exchange = %Exchange::OKX, count, "Preloaded symbol metas");
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

    /// 创建指定交易所的 Actor（无条件创建）
    async fn create_exchange_actor(
        &mut self,
        exchange: Exchange,
        actor_ref: &ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        let processor = self.processor.as_ref().ok_or_else(|| {
            ExchangeError::Other("ProcessorActor not initialized".to_string())
        })?;

        // 创建 ProcessorEventSink（实现 EventSink，发送到 ProcessorActor）
        let event_sink: Arc<dyn EventSink> = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });

        match exchange {
            Exchange::Binance => {
                let client = self.binance_client.as_ref().ok_or_else(|| {
                    ExchangeError::Other("Binance client not configured".to_string())
                })?;

                // 提取该交易所的 symbol metas
                let symbol_metas: HashMap<Symbol, SymbolMeta> = self
                    .symbol_metas
                    .iter()
                    .filter(|((e, _), _)| *e == Exchange::Binance)
                    .map(|((_, s), m)| (s.clone(), m.clone()))
                    .collect();

                let actor = BinanceActor::new(BinanceActorArgs {
                    credentials: client.credentials().cloned(),
                    symbol_metas: Arc::new(symbol_metas),
                    event_sink,
                    rest_base_url: client.rest_base_url().to_string(),
                });

                let binance_ref = spawn_link(actor_ref, actor).await;
                self.actor_kinds
                    .insert(binance_ref.id(), ChildActorKind::Exchange(Exchange::Binance));
                self.binance_actor = Some(binance_ref);

                tracing::info!("BinanceActor created");
            }
            Exchange::OKX => {
                let client = self.okx_client.as_ref().ok_or_else(|| {
                    ExchangeError::Other("OKX client not configured".to_string())
                })?;

                // 提取该交易所的 symbol metas
                let symbol_metas: HashMap<Symbol, SymbolMeta> = self
                    .symbol_metas
                    .iter()
                    .filter(|((e, _), _)| *e == Exchange::OKX)
                    .map(|((_, s), m)| (s.clone(), m.clone()))
                    .collect();

                let actor = OkxActor::new(OkxActorArgs {
                    credentials: client.credentials().cloned(),
                    symbol_metas: Arc::new(symbol_metas),
                    event_sink,
                });

                let okx_ref = spawn_link(actor_ref, actor).await;
                self.actor_kinds
                    .insert(okx_ref.id(), ChildActorKind::Exchange(Exchange::OKX));
                self.okx_actor = Some(okx_ref);

                tracing::info!("OkxActor created");
            }
        }

        Ok(())
    }

    /// 确保 ExchangeActor 存在（如不存在则创建）
    async fn ensure_exchange_actor(
        &mut self,
        exchange: Exchange,
        actor_ref: &ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        let exists = match exchange {
            Exchange::Binance => self.binance_actor.is_some(),
            Exchange::OKX => self.okx_actor.is_some(),
        };

        if !exists {
            self.create_exchange_actor(exchange, actor_ref).await?;
        }

        Ok(())
    }

    /// 添加策略的内部实现
    async fn do_add_strategy(&mut self, strategy: Box<dyn Strategy>) -> Result<(), ExchangeError> {
        let actor_ref = self.self_ref.clone().ok_or_else(|| {
            ExchangeError::Other("self_ref not set".to_string())
        })?;

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

        // 3. 启动缺失的 ExchangeActors
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
            match exchange {
                Exchange::Binance => {
                    if let Some(actor) = &self.binance_actor {
                        actor
                            .tell(Subscribe { kind })
                            .await
                            .expect("Failed to subscribe to BinanceActor");
                    }
                }
                Exchange::OKX => {
                    if let Some(actor) = &self.okx_actor {
                        actor
                            .tell(Subscribe { kind })
                            .await
                            .expect("Failed to subscribe to OkxActor");
                    }
                }
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
        self.self_ref = Some(actor_ref.clone());

        // 1. 预加载所有交易所的 symbol metas
        self.preload_symbol_metas().await?;

        // 2. 创建 ProcessorActor（先创建，因为其他 Actor 需要它的引用）
        let processor = spawn_link(&actor_ref, ProcessorActor::new()).await;
        self.actor_kinds
            .insert(processor.id(), ChildActorKind::Processor);
        self.processor = Some(processor.clone());

        // 创建 ProcessorEventSink（供其他 Actor 发送事件）
        let event_sink: Arc<dyn EventSink> = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });

        // 3. 创建 SignalProcessorActor
        let clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = [
            self.binance_client.clone().map(|c| (Exchange::Binance, c as Arc<dyn ExchangeClient>)),
            self.okx_client.clone().map(|c| (Exchange::OKX, c as Arc<dyn ExchangeClient>)),
        ]
        .into_iter()
        .flatten()
        .collect();

        let signal_processor = spawn_link(
            &actor_ref,
            SignalProcessorActor::new(super::SignalProcessorArgs {
                clients,
                event_sink: event_sink.clone(),
            }),
        )
        .await;
        self.actor_kinds
            .insert(signal_processor.id(), ChildActorKind::SignalProcessor);
        self.signal_processor = Some(signal_processor);

        // 4. 创建 ClockActor（使用具体类型 ProcessorEventSink）
        let clock_event_sink = Arc::new(ProcessorEventSink {
            processor: processor.clone(),
        });
        let clock_actor = spawn_link(
            &actor_ref,
            ClockActor::new(ClockArgs {
                interval_ms: 1000, // 1 秒
                binance_client: self.binance_client.clone().map(|c| c as Arc<dyn ExchangeClient>),
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
                    exchange = %exchange,
                    reason = ?reason,
                    "ExchangeActor died, shutting down"
                );
                match exchange {
                    Exchange::Binance => self.binance_actor = None,
                    Exchange::OKX => self.okx_actor = None,
                }
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
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.do_add_strategy(msg.0).await
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
struct ProcessorEventSink {
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


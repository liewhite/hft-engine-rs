//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 使用 spawn_link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use super::{ExecutorActor, ExecutorArgs, ProcessorActor, RegisterExecutor, SignalProcessorActor};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{BinanceActor, BinanceActorArgs, BinanceClient};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxClient};
use crate::exchange::{ExchangeClient, MarketDataSink, SignalSink, Subscribe, SubscriptionKind};
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
            binance_actor: None,
            okx_actor: None,
            executors: Vec::new(),
            actor_kinds: HashMap::new(),
            self_ref: None,
        }
    }

    /// 获取交易所客户端
    fn get_exchange_client(&self, exchange: Exchange) -> Option<Arc<dyn ExchangeClient>> {
        match exchange {
            Exchange::Binance => self.binance_client.clone().map(|c| c as Arc<dyn ExchangeClient>),
            Exchange::OKX => self.okx_client.clone().map(|c| c as Arc<dyn ExchangeClient>),
        }
    }

    /// 获取或获取 SymbolMetas
    async fn ensure_symbol_metas(
        &mut self,
        exchange: Exchange,
        symbols: &[Symbol],
    ) -> Result<(), ExchangeError> {
        // 检查哪些 symbols 需要获取
        let missing: Vec<Symbol> = symbols
            .iter()
            .filter(|s| !self.symbol_metas.contains_key(&(exchange, (*s).clone())))
            .cloned()
            .collect();

        if missing.is_empty() {
            return Ok(());
        }

        let client = self.get_exchange_client(exchange).ok_or_else(|| {
            ExchangeError::Other(format!("Exchange {:?} client not registered", exchange))
        })?;

        let metas = client.fetch_symbol_meta(&missing).await?;
        for meta in metas {
            self.symbol_metas.insert((exchange, meta.symbol.clone()), meta);
        }

        tracing::info!(
            exchange = %exchange,
            fetched = missing.len(),
            "Fetched symbol metas"
        );

        Ok(())
    }

    /// 确保 ExchangeActor 存在
    async fn ensure_exchange_actor(
        &mut self,
        exchange: Exchange,
        actor_ref: &ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        let processor = self.processor.as_ref().ok_or_else(|| {
            ExchangeError::Other("ProcessorActor not initialized".to_string())
        })?;

        // 创建 ProcessorDataSink（实现 MarketDataSink，发送到 ProcessorActor）
        let data_sink: Arc<dyn MarketDataSink> = Arc::new(ProcessorDataSink {
            processor: processor.clone(),
        });

        match exchange {
            Exchange::Binance if self.binance_actor.is_none() => {
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
                    data_sink,
                    rest_base_url: client.rest_base_url().to_string(),
                });

                let binance_ref = spawn_link(actor_ref, actor).await;
                self.actor_kinds
                    .insert(binance_ref.id(), ChildActorKind::Exchange(Exchange::Binance));
                self.binance_actor = Some(binance_ref);

                tracing::info!("BinanceActor created");
            }
            Exchange::OKX if self.okx_actor.is_none() => {
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
                    data_sink,
                });

                let okx_ref = spawn_link(actor_ref, actor).await;
                self.actor_kinds
                    .insert(okx_ref.id(), ChildActorKind::Exchange(Exchange::OKX));
                self.okx_actor = Some(okx_ref);

                tracing::info!("OkxActor created");
            }
            _ => {} // 已存在或不需要
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

        // 1. 获取策略需要的 public streams
        let public_streams = strategy.public_streams();

        // 2. 获取所需的 SymbolMetas
        for (exchange, symbol_streams) in &public_streams {
            let symbols: Vec<Symbol> = symbol_streams.keys().cloned().collect();
            self.ensure_symbol_metas(*exchange, &symbols).await?;
        }

        // 3. 启动缺失的 ExchangeActors
        for exchange in public_streams.keys() {
            self.ensure_exchange_actor(*exchange, &actor_ref).await?;
        }

        // 4. 收集策略需要的订阅
        let mut subscriptions: HashSet<(Exchange, SubscriptionKind)> = HashSet::new();
        for (exchange, symbol_streams) in &public_streams {
            for (symbol, data_types) in symbol_streams {
                for data_type in data_types {
                    let kind = data_type.to_subscription_kind(symbol.clone());
                    subscriptions.insert((*exchange, kind));
                }
            }
        }

        // 5. 创建 ExecutorActor
        let executor_idx = self.executors.len();

        // 创建 SignalSink（发送到 SignalProcessorActor）
        let signal_sink: Arc<dyn SignalSink> = Arc::new(SignalProcessorSink {
            signal_processor: signal_processor.clone(),
        });

        let executor = ExecutorActor::new(ExecutorArgs {
            strategy,
            symbol_metas: Arc::new(self.symbol_metas.clone()),
            signal_sink,
        });

        let executor_ref = spawn_link(&actor_ref, executor).await;
        self.actor_kinds
            .insert(executor_ref.id(), ChildActorKind::Executor(executor_idx));

        // 6. 向 ProcessorActor 注册 Executor 的订阅
        let _ = processor
            .tell(RegisterExecutor {
                executor: executor_ref.clone(),
                subscriptions: subscriptions.clone(),
            })
            .await;

        self.executors.push(executor_ref);

        // 7. 向 ExchangeActors 发送订阅请求
        for (exchange, kind) in subscriptions {
            match exchange {
                Exchange::Binance => {
                    if let Some(actor) = &self.binance_actor {
                        let _ = actor.tell(Subscribe { kind }).await;
                    }
                }
                Exchange::OKX => {
                    if let Some(actor) = &self.okx_actor {
                        let _ = actor.tell(Subscribe { kind }).await;
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

        // 1. 创建 SignalProcessorActor
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
                executors: clients,
            }),
        )
        .await;
        self.actor_kinds
            .insert(signal_processor.id(), ChildActorKind::SignalProcessor);
        self.signal_processor = Some(signal_processor);

        // 2. 创建 ProcessorActor
        let processor = spawn_link(&actor_ref, ProcessorActor::new()).await;
        self.actor_kinds
            .insert(processor.id(), ChildActorKind::Processor);
        self.processor = Some(processor);

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

/// ProcessorActor 的数据接收器
struct ProcessorDataSink {
    processor: ActorRef<ProcessorActor>,
}

#[async_trait]
impl MarketDataSink for ProcessorDataSink {
    async fn send_market_data(&self, data: crate::exchange::MarketData) {
        let _ = self.processor.tell(data).await;
    }
}

/// SignalProcessorActor 的信号接收器
struct SignalProcessorSink {
    signal_processor: ActorRef<SignalProcessorActor>,
}

#[async_trait]
impl SignalSink for SignalProcessorSink {
    async fn send_signal(&self, signal: crate::strategy::Signal) {
        let _ = self.signal_processor.tell(signal).await;
    }
}

// 为了向后兼容，保留旧名称的别名
pub type EngineActor = ManagerActor;
pub type EngineActorArgs = ManagerActorArgs;

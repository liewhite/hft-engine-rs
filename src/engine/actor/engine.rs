//! EngineActor - 顶层 Actor，管理所有子 Actor
//!
//! 职责：生命周期编排、策略管理、数据分发
//!
//! Supervisor 职责：
//! - 使用 spawn_link 创建所有子 Actor
//! - on_link_died 时根据 Actor 类型决定处理方式

use super::{
    ClockActor, ClockArgs, ExecutorActor, ExecutorArgs, RegisterExecutor, SignalProcessorActor,
    SignalProcessorArgs,
};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::{
    ExchangeClient, ExchangeClientHandle, MarketData, MarketDataSink, PublicDataType,
};
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::strategy::{PublicStreams, Strategy};

/// EngineActor 的数据接收器 (实现 MarketDataSink)
pub struct EngineDataSink {
    engine_ref: WeakActorRef<EngineActor>,
}

#[async_trait]
impl MarketDataSink for EngineDataSink {
    async fn send_market_data(&self, data: MarketData) {
        if let Some(actor) = self.engine_ref.upgrade() {
            let _ = actor.tell(data).await;
        }
    }
}

// === 子 Actor 类型标识 ===

/// 子 Actor 类型（用于 on_link_died 中识别）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildActorKind {
    Exchange(Exchange),
    Executor(usize), // index
    SignalProcessor,
    Clock,
}

// === EngineActor ===

/// EngineActor 初始化参数
pub struct EngineActorArgs {
    /// 交易所客户端
    pub exchanges: HashMap<Exchange, Arc<dyn ExchangeClient>>,
}

/// EngineActor - 顶层管理 Actor
pub struct EngineActor {
    /// 交易所客户端
    exchanges: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 策略列表
    strategies: Vec<Box<dyn Strategy>>,
    /// Symbol 元数据
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,

    /// ExchangeClientHandle (类型擦除)
    exchange_handles: HashMap<Exchange, Arc<dyn ExchangeClientHandle>>,
    /// ExecutorActors
    executor_actors: Vec<ActorRef<ExecutorActor>>,
    /// SignalProcessorActor
    signal_processor: Option<ActorRef<SignalProcessorActor>>,
    /// ClockActor
    clock: Option<ActorRef<ClockActor<EngineDataSink>>>,

    /// ActorID -> ChildActorKind 映射（用于 on_link_died）
    actor_kinds: HashMap<ActorID, ChildActorKind>,

    /// Actor 自身引用
    self_ref: Option<ActorRef<Self>>,
}

impl EngineActor {
    pub fn new(args: EngineActorArgs) -> Self {
        Self {
            exchanges: args.exchanges,
            strategies: Vec::new(),
            symbol_metas: HashMap::new(),
            exchange_handles: HashMap::new(),
            executor_actors: Vec::new(),
            signal_processor: None,
            clock: None,
            actor_kinds: HashMap::new(),
            self_ref: None,
        }
    }

    /// 获取 SymbolMeta
    async fn fetch_symbol_metas(
        &mut self,
        aggregated: &PublicStreams,
    ) -> Result<(), ExchangeError> {
        for (exchange, symbol_streams) in aggregated {
            let client = self.exchanges.get(exchange).ok_or_else(|| {
                ExchangeError::Other(format!("Exchange {:?} client not registered", exchange))
            })?;

            let symbols: Vec<Symbol> = symbol_streams.keys().cloned().collect();
            let metas = client.fetch_symbol_meta(&symbols).await?;

            for meta in metas {
                self.symbol_metas
                    .insert((*exchange, meta.symbol.clone()), meta);
            }

            tracing::info!(
                exchange = %exchange,
                symbols = symbols.len(),
                metas = self.symbol_metas.iter().filter(|((e, _), _)| e == exchange).count(),
                "Fetched symbol metas"
            );
        }

        Ok(())
    }

    /// 启动 ExchangeClient 并获取 Handle
    async fn start_exchange_client(
        &mut self,
        actor_ref: &ActorRef<Self>,
        exchange: Exchange,
    ) -> Result<(), ExchangeError> {
        let client = self.exchanges.get(&exchange).ok_or_else(|| {
            ExchangeError::Other(format!("Exchange {:?} client not found", exchange))
        })?;

        // 提取该交易所的 symbol metas
        let symbol_metas: HashMap<Symbol, SymbolMeta> = self
            .symbol_metas
            .iter()
            .filter(|((e, _), _)| *e == exchange)
            .map(|((_, s), m)| (s.clone(), m.clone()))
            .collect();

        // 创建 EngineDataSink (ExchangeActor → EngineActor)
        let data_sink: Arc<dyn MarketDataSink> = Arc::new(EngineDataSink {
            engine_ref: actor_ref.downgrade(),
        });

        let handle = client
            .clone()
            .start(Arc::new(symbol_metas), data_sink)
            .await?;

        // 记录 ActorID -> Kind 映射
        self.actor_kinds
            .insert(handle.actor_id(), ChildActorKind::Exchange(exchange));

        self.exchange_handles.insert(exchange, handle);
        Ok(())
    }

    /// 发送订阅请求
    async fn send_subscriptions(
        handle: &dyn ExchangeClientHandle,
        symbol_streams: &HashMap<Symbol, HashSet<PublicDataType>>,
    ) {
        for (symbol, data_types) in symbol_streams {
            for data_type in data_types {
                let kind = data_type.to_subscription_kind(symbol.clone());
                handle.subscribe(kind).await;
            }
        }
    }

    /// 聚合所有策略需要的 public streams
    fn aggregate_public_streams(strategies: &[Box<dyn Strategy>]) -> PublicStreams {
        let mut aggregated: PublicStreams = HashMap::new();

        for strategy in strategies {
            for (exchange, symbol_streams) in strategy.public_streams() {
                let exchange_entry = aggregated.entry(exchange).or_default();
                for (symbol, data_types) in symbol_streams {
                    exchange_entry.entry(symbol).or_default().extend(data_types);
                }
            }
        }

        aggregated
    }
}

impl Actor for EngineActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "EngineActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.clone());
        tracing::info!("EngineActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // 链接的子 Actor 会自动收到通知并停止
        tracing::info!(reason = ?reason, "EngineActor stopped");
        Ok(())
    }

    /// 处理链接的 Actor 死亡
    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorID,
        reason: ActorStopReason,
    ) -> Result<Option<ActorStopReason>, BoxError> {
        // 查找死掉的 Actor 类型
        let kind = self.actor_kinds.remove(&id);

        match kind {
            Some(ChildActorKind::Exchange(exchange)) => {
                // Exchange 死亡是致命的，级联停止整个引擎
                tracing::error!(
                    exchange = %exchange,
                    reason = ?reason,
                    "ExchangeActor died, shutting down engine"
                );
                self.exchange_handles.remove(&exchange);
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::Executor(idx)) => {
                // Executor 死亡是致命的
                tracing::error!(
                    executor_idx = idx,
                    reason = ?reason,
                    "ExecutorActor died, shutting down engine"
                );
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::SignalProcessor) => {
                // SignalProcessor 死亡是致命的
                tracing::error!(
                    reason = ?reason,
                    "SignalProcessorActor died, shutting down engine"
                );
                self.signal_processor = None;
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            Some(ChildActorKind::Clock) => {
                // Clock 死亡是致命的
                tracing::error!(
                    reason = ?reason,
                    "ClockActor died, shutting down engine"
                );
                self.clock = None;
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
            None => {
                // 未知的 Actor
                tracing::warn!(
                    actor_id = ?id,
                    reason = ?reason,
                    "Unknown linked actor died"
                );
                Ok(None)
            }
        }
    }
}

// === Messages ===

/// 添加策略
pub struct AddStrategy(pub Box<dyn Strategy>);

impl Message<AddStrategy> for EngineActor {
    type Reply = ();

    async fn handle(&mut self, msg: AddStrategy, _ctx: Context<'_, Self, Self::Reply>) {
        self.strategies.push(msg.0);
    }
}

/// 启动引擎
pub struct Start;

impl Message<Start> for EngineActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Start, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        if let Err(e) = self.do_start().await {
            tracing::error!(error = %e, "Failed to start engine");
        }
    }
}

impl EngineActor {
    /// 实际启动逻辑
    async fn do_start(&mut self) -> Result<(), ExchangeError> {
        let actor_ref = self
            .self_ref
            .clone()
            .ok_or_else(|| ExchangeError::Other("self_ref not set".to_string()))?;

        // 1. 聚合所有策略需要的 public streams
        let aggregated = Self::aggregate_public_streams(&self.strategies);
        let required_exchanges: HashSet<Exchange> = aggregated.keys().cloned().collect();

        tracing::info!(
            exchanges = aggregated.len(),
            strategies = self.strategies.len(),
            "Starting engine"
        );

        // 2. 获取 SymbolMeta
        self.fetch_symbol_metas(&aggregated).await?;

        // 3. 启动 ExchangeClient (数据通过 EngineDataSink 发送到 EngineActor)
        for exchange in &required_exchanges {
            self.start_exchange_client(&actor_ref, *exchange).await?;
        }

        // 4. 发送订阅请求
        for (exchange, symbol_streams) in &aggregated {
            if let Some(handle) = self.exchange_handles.get(exchange) {
                Self::send_subscriptions(handle.as_ref(), symbol_streams).await;
                tracing::info!(
                    exchange = %exchange,
                    symbols = symbol_streams.len(),
                    "Subscriptions sent"
                );
            }
        }

        // 5. 使用 spawn_link 创建 SignalProcessorActor
        let signal_processor = spawn_link(
            &actor_ref,
            SignalProcessorActor::new(SignalProcessorArgs {
                executors: self.exchanges.clone(),
            }),
        )
        .await;
        self.actor_kinds
            .insert(signal_processor.id(), ChildActorKind::SignalProcessor);
        self.signal_processor = Some(signal_processor.clone());

        // 6. 使用 spawn_link 创建 ExecutorActors (Signal 直接发送到 SignalProcessorActor)
        let symbol_metas = Arc::new(self.symbol_metas.clone());
        let strategies = std::mem::take(&mut self.strategies);

        for (idx, strategy) in strategies.into_iter().enumerate() {
            let executor = spawn_link(
                &actor_ref,
                ExecutorActor::new(ExecutorArgs {
                    strategy,
                    symbol_metas: symbol_metas.clone(),
                    signal_processor: signal_processor.clone(),
                }),
            )
            .await;
            self.actor_kinds
                .insert(executor.id(), ChildActorKind::Executor(idx));
            self.executor_actors.push(executor);
        }

        // 7. 使用 spawn_link 创建 ClockActor (Equity 数据通过 EngineDataSink 发送到 EngineActor)
        let binance_client = self.exchanges.get(&Exchange::Binance).cloned();
        let data_sink = Arc::new(EngineDataSink {
            engine_ref: actor_ref.downgrade(),
        });
        let clock = spawn_link(
            &actor_ref,
            ClockActor::new(ClockArgs {
                interval_ms: 1000,
                binance_client,
                data_sink,
            }),
        )
        .await;
        self.actor_kinds.insert(clock.id(), ChildActorKind::Clock);
        self.clock = Some(clock.clone());

        // 8. 注册所有 Executor 到 Clock
        for executor in &self.executor_actors {
            let _ = clock
                .tell(RegisterExecutor {
                    executor: executor.clone(),
                })
                .await;
        }

        tracing::info!("Engine started successfully");
        Ok(())
    }
}

/// 停止引擎
pub struct Stop;

impl Message<Stop> for EngineActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Stop, ctx: Context<'_, Self, Self::Reply>) {
        tracing::info!("Stopping engine...");
        ctx.actor_ref().stop_gracefully().await.ok();
    }
}

/// MarketData 处理 - 广播到所有 ExecutorActor
impl Message<MarketData> for EngineActor {
    type Reply = ();

    async fn handle(&mut self, msg: MarketData, _ctx: Context<'_, Self, Self::Reply>) {
        // 广播到所有 ExecutorActor
        for executor in &self.executor_actors {
            let _ = executor.tell(msg.clone()).await;
        }
    }
}

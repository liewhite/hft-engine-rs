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
use crate::config::ExchangesConfig;
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::actor::{ExchangeActor, ExchangeActorArgs, MarketDataSink};
use crate::exchange::binance::{BinanceConfig, BinanceCredentials, BinanceRestClient};
use crate::exchange::okx::{OkxConfig, OkxCredentials, OkxRestClient};
use crate::exchange::subscriber::{MarketData, Subscribe, SubscriptionKind};
use crate::exchange::ExchangeConfig;
use crate::exchange::{ExchangeExecutor, PublicDataType};
use crate::strategy::{PublicStreams, Strategy};
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// EngineActor 的数据接收器 (实现 MarketDataSink)
struct EngineDataSink {
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

// === ExchangeActorHandle trait - 消除 match exchange 分发 ===

/// ExchangeActor 的类型擦除句柄
#[async_trait]
trait ExchangeActorHandle: Send + Sync {
    /// 获取 Actor ID
    fn actor_id(&self) -> ActorID;
    /// 发送订阅请求
    async fn subscribe(&self, kind: SubscriptionKind);
    /// 停止 Actor
    async fn stop(&self);
}

/// 为 ActorRef<ExchangeActor<C, S>> 实现 ExchangeActorHandle
#[async_trait]
impl<C: ExchangeConfig, S: MarketDataSink> ExchangeActorHandle
    for ActorRef<ExchangeActor<C, S>>
{
    fn actor_id(&self) -> ActorID {
        self.id()
    }

    async fn subscribe(&self, kind: SubscriptionKind) {
        if let Err(e) = self.tell(Subscribe { kind }).await {
            tracing::error!(error = %e, "Failed to send Subscribe message");
        }
    }

    async fn stop(&self) {
        self.stop_gracefully().await.ok();
    }
}

// === ExchangeFactory trait - 创建交易所 Actor 的工厂 ===

/// 交易所工厂 trait
#[async_trait]
trait ExchangeFactory: Send + Sync {
    /// 创建 REST 客户端
    fn create_rest_client(&self) -> Result<Arc<dyn ExchangeExecutor>, ExchangeError>;
    /// 使用 spawn_link 创建 ExchangeActor
    async fn create_actor_linked(
        &self,
        engine_ref: &ActorRef<EngineActor>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: Arc<EngineDataSink>,
    ) -> Arc<dyn ExchangeActorHandle>;
}

/// Binance 工厂
struct BinanceFactory {
    api_key: String,
    secret: String,
}

#[async_trait]
impl ExchangeFactory for BinanceFactory {
    fn create_rest_client(&self) -> Result<Arc<dyn ExchangeExecutor>, ExchangeError> {
        Ok(Arc::new(BinanceRestClient::new(
            self.api_key.clone(),
            self.secret.clone(),
        )?))
    }

    async fn create_actor_linked(
        &self,
        engine_ref: &ActorRef<EngineActor>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: Arc<EngineDataSink>,
    ) -> Arc<dyn ExchangeActorHandle> {
        let credentials = BinanceCredentials {
            api_key: self.api_key.clone(),
            secret: self.secret.clone(),
            listen_key: None,
        };

        let actor = spawn_link(
            engine_ref,
            ExchangeActor::<BinanceConfig, EngineDataSink>::new(ExchangeActorArgs {
                symbol_metas,
                credentials,
                data_sink,
            }),
        )
        .await;

        Arc::new(actor)
    }
}

/// OKX 工厂
struct OkxFactory {
    api_key: String,
    secret: String,
    passphrase: String,
}

#[async_trait]
impl ExchangeFactory for OkxFactory {
    fn create_rest_client(&self) -> Result<Arc<dyn ExchangeExecutor>, ExchangeError> {
        Ok(Arc::new(OkxRestClient::new(
            self.api_key.clone(),
            self.secret.clone(),
            self.passphrase.clone(),
        )?))
    }

    async fn create_actor_linked(
        &self,
        engine_ref: &ActorRef<EngineActor>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: Arc<EngineDataSink>,
    ) -> Arc<dyn ExchangeActorHandle> {
        let credentials = OkxCredentials {
            api_key: self.api_key.clone(),
            secret: self.secret.clone(),
            passphrase: self.passphrase.clone(),
        };

        let actor = spawn_link(
            engine_ref,
            ExchangeActor::<OkxConfig, EngineDataSink>::new(ExchangeActorArgs {
                symbol_metas,
                credentials,
                data_sink,
            }),
        )
        .await;

        Arc::new(actor)
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
    pub exchanges_config: ExchangesConfig,
}

/// EngineActor - 顶层管理 Actor
pub struct EngineActor {
    /// 交易所工厂
    factories: HashMap<Exchange, Box<dyn ExchangeFactory>>,
    /// 策略列表
    strategies: Vec<Box<dyn Strategy>>,
    /// Symbol 元数据
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,
    /// REST 客户端
    rests: HashMap<Exchange, Arc<dyn ExchangeExecutor>>,

    /// ExchangeActors (类型擦除)
    exchange_actors: HashMap<Exchange, Arc<dyn ExchangeActorHandle>>,
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
        // 创建工厂
        let mut factories: HashMap<Exchange, Box<dyn ExchangeFactory>> = HashMap::new();

        factories.insert(
            Exchange::Binance,
            Box::new(BinanceFactory {
                api_key: args.exchanges_config.binance.api_key.clone(),
                secret: args.exchanges_config.binance.secret.clone(),
            }),
        );

        factories.insert(
            Exchange::OKX,
            Box::new(OkxFactory {
                api_key: args.exchanges_config.okx.api_key.clone(),
                secret: args.exchanges_config.okx.secret.clone(),
                passphrase: args.exchanges_config.okx.passphrase.clone(),
            }),
        );

        Self {
            factories,
            strategies: Vec::new(),
            symbol_metas: HashMap::new(),
            rests: HashMap::new(),
            exchange_actors: HashMap::new(),
            executor_actors: Vec::new(),
            signal_processor: None,
            clock: None,
            actor_kinds: HashMap::new(),
            self_ref: None,
        }
    }

    /// 初始化 REST 客户端 (无 match 分发)
    fn init_rest_clients(&mut self, exchanges: &HashSet<Exchange>) -> Result<(), ExchangeError> {
        for exchange in exchanges {
            if self.rests.contains_key(exchange) {
                continue;
            }

            let factory = self.factories.get(exchange).ok_or_else(|| {
                ExchangeError::Other(format!("No factory for exchange {:?}", exchange))
            })?;

            let rest = factory.create_rest_client()?;
            self.rests.insert(*exchange, rest);

            tracing::info!(exchange = %exchange, "REST client initialized");
        }

        Ok(())
    }

    /// 获取 SymbolMeta
    async fn fetch_symbol_metas(
        &mut self,
        aggregated: &PublicStreams,
    ) -> Result<(), ExchangeError> {
        for (exchange, symbol_streams) in aggregated {
            let executor = self.rests.get(exchange).ok_or_else(|| {
                ExchangeError::Other(format!("Exchange {:?} executor not registered", exchange))
            })?;

            let symbols: Vec<Symbol> = symbol_streams.keys().cloned().collect();
            let metas = executor.fetch_symbol_meta(&symbols).await?;

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

    /// 使用 spawn_link 创建 ExchangeActor
    async fn create_exchange_actor_linked(
        &mut self,
        actor_ref: &ActorRef<Self>,
        exchange: Exchange,
    ) -> Result<(), ExchangeError> {
        let factory = self.factories.get(&exchange).ok_or_else(|| {
            ExchangeError::Other(format!("No factory for exchange {:?}", exchange))
        })?;

        // 提取该交易所的 symbol metas
        let symbol_metas: HashMap<Symbol, SymbolMeta> = self
            .symbol_metas
            .iter()
            .filter(|((e, _), _)| *e == exchange)
            .map(|((_, s), m)| (s.clone(), m.clone()))
            .collect();

        // 创建 EngineDataSink (ExchangeActor → EngineActor)
        let data_sink = Arc::new(EngineDataSink {
            engine_ref: actor_ref.downgrade(),
        });

        let actor = factory
            .create_actor_linked(actor_ref, Arc::new(symbol_metas), data_sink)
            .await;

        // 记录 ActorID -> Kind 映射
        self.actor_kinds
            .insert(actor.actor_id(), ChildActorKind::Exchange(exchange));

        self.exchange_actors.insert(exchange, actor);
        Ok(())
    }

    /// 发送订阅请求 (通过 PublicDataType::to_subscription_kind 消除 if 分发)
    async fn send_subscriptions(
        actor: &dyn ExchangeActorHandle,
        symbol_streams: &HashMap<Symbol, HashSet<PublicDataType>>,
    ) {
        for (symbol, data_types) in symbol_streams {
            for data_type in data_types {
                let kind = data_type.to_subscription_kind(symbol.clone());
                actor.subscribe(kind).await;
            }
        }

        // 订阅 private 数据
        actor.subscribe(SubscriptionKind::Private).await;
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
                self.exchange_actors.remove(&exchange);
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
        let actor_ref = self.self_ref.clone().ok_or_else(|| {
            ExchangeError::Other("self_ref not set".to_string())
        })?;

        // 1. 聚合所有策略需要的 public streams
        let aggregated = Self::aggregate_public_streams(&self.strategies);
        let required_exchanges: HashSet<Exchange> = aggregated.keys().cloned().collect();

        tracing::info!(
            exchanges = aggregated.len(),
            strategies = self.strategies.len(),
            "Starting engine"
        );

        // 2. 初始化 REST 客户端并获取 SymbolMeta
        self.init_rest_clients(&required_exchanges)?;
        self.fetch_symbol_metas(&aggregated).await?;

        // 3. 使用 spawn_link 创建 ExchangeActors (数据通过 EngineDataSink 发送到 EngineActor)
        for exchange in &required_exchanges {
            self.create_exchange_actor_linked(&actor_ref, *exchange).await?;
        }

        // 4. 发送订阅请求 (无 match 分发)
        for (exchange, symbol_streams) in &aggregated {
            if let Some(actor) = self.exchange_actors.get(exchange) {
                Self::send_subscriptions(actor.as_ref(), symbol_streams).await;
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
                executors: self.rests.clone(),
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
        let binance_executor = self.rests.get(&Exchange::Binance).cloned();
        let data_sink = Arc::new(EngineDataSink {
            engine_ref: actor_ref.downgrade(),
        });
        let clock = spawn_link(
            &actor_ref,
            ClockActor::new(ClockArgs {
                interval_ms: 1000,
                binance_executor,
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

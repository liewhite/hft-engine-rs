//! EngineActor - 顶层 Actor，管理所有子 Actor
//!
//! 职责：生命周期编排、策略管理、数据分发

use super::{
    ClockActor, ClockArgs, ExecutorActor, ExecutorArgs, RegisterExecutor, SignalProcessorActor,
    SignalProcessorArgs,
};
use crate::config::ExchangesConfig;
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::actor::{ExchangeActor, ExchangeActorArgs};
use crate::exchange::binance::{BinanceConfig, BinanceCredentials, BinanceRestClient};
use crate::exchange::okx::{OkxConfig, OkxCredentials, OkxRestClient};
use crate::exchange::subscriber::{MarketData, Subscribe, SubscriptionKind};
use crate::exchange::ExchangeConfig;
use crate::exchange::{ExchangeExecutor, PublicDataType};
use crate::strategy::{PublicStreams, Signal, Strategy};
use async_trait::async_trait;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

// === ExchangeActorHandle trait - 消除 match exchange 分发 ===

/// ExchangeActor 的类型擦除句柄
#[async_trait]
trait ExchangeActorHandle: Send + Sync {
    /// 发送订阅请求
    async fn subscribe(&self, kind: SubscriptionKind);
    /// 停止 Actor
    async fn stop(&self);
}

/// 为 ActorRef<ExchangeActor<C>> 实现 ExchangeActorHandle
#[async_trait]
impl<C: ExchangeConfig> ExchangeActorHandle for ActorRef<ExchangeActor<C>> {
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
trait ExchangeFactory {
    /// 创建 REST 客户端
    fn create_rest_client(&self) -> Result<Arc<dyn ExchangeExecutor>, ExchangeError>;
    /// 创建 ExchangeActor
    fn create_actor(
        &self,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: mpsc::Sender<MarketData>,
    ) -> Arc<dyn ExchangeActorHandle>;
}

/// Binance 工厂
struct BinanceFactory {
    api_key: String,
    secret: String,
}

impl ExchangeFactory for BinanceFactory {
    fn create_rest_client(&self) -> Result<Arc<dyn ExchangeExecutor>, ExchangeError> {
        Ok(Arc::new(BinanceRestClient::new(
            self.api_key.clone(),
            self.secret.clone(),
        )?))
    }

    fn create_actor(
        &self,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: mpsc::Sender<MarketData>,
    ) -> Arc<dyn ExchangeActorHandle> {
        let credentials = BinanceCredentials {
            api_key: self.api_key.clone(),
            secret: self.secret.clone(),
            listen_key: None,
        };

        let actor = kameo::spawn(ExchangeActor::<BinanceConfig>::new(ExchangeActorArgs {
            symbol_metas,
            credentials,
            data_sink,
        }));

        Arc::new(actor)
    }
}

/// OKX 工厂
struct OkxFactory {
    api_key: String,
    secret: String,
    passphrase: String,
}

impl ExchangeFactory for OkxFactory {
    fn create_rest_client(&self) -> Result<Arc<dyn ExchangeExecutor>, ExchangeError> {
        Ok(Arc::new(OkxRestClient::new(
            self.api_key.clone(),
            self.secret.clone(),
            self.passphrase.clone(),
        )?))
    }

    fn create_actor(
        &self,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: mpsc::Sender<MarketData>,
    ) -> Arc<dyn ExchangeActorHandle> {
        let credentials = OkxCredentials {
            api_key: self.api_key.clone(),
            secret: self.secret.clone(),
            passphrase: self.passphrase.clone(),
        };

        let actor = kameo::spawn(ExchangeActor::<OkxConfig>::new(ExchangeActorArgs {
            symbol_metas,
            credentials,
            data_sink,
        }));

        Arc::new(actor)
    }
}

// === EngineActor ===

/// EngineActor 初始化参数
pub struct EngineActorArgs {
    pub exchanges_config: ExchangesConfig,
}

/// EngineActor - 顶层管理 Actor
pub struct EngineActor {
    /// 交易所工厂
    factories: HashMap<Exchange, Box<dyn ExchangeFactory + Send + Sync>>,
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
    clock: Option<ActorRef<ClockActor>>,

    /// MarketData 分发任务
    dispatcher_task: Option<tokio::task::JoinHandle<()>>,
    /// Signal 转发任务
    forwarder_task: Option<tokio::task::JoinHandle<()>>,

    /// Actor 自身引用
    self_ref: Option<WeakActorRef<Self>>,
}

impl EngineActor {
    pub fn new(args: EngineActorArgs) -> Self {
        // 创建工厂
        let mut factories: HashMap<Exchange, Box<dyn ExchangeFactory + Send + Sync>> =
            HashMap::new();

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
            dispatcher_task: None,
            forwarder_task: None,
            self_ref: None,
        }
    }

    /// 初始化 REST 客户端 (无 match 分发)
    fn init_rest_clients(
        &mut self,
        exchanges: &HashSet<Exchange>,
    ) -> Result<(), ExchangeError> {
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

    /// 创建 ExchangeActor (无 match 分发)
    fn create_exchange_actor(
        &self,
        exchange: Exchange,
        data_tx: mpsc::Sender<MarketData>,
    ) -> Result<Arc<dyn ExchangeActorHandle>, ExchangeError> {
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

        Ok(factory.create_actor(Arc::new(symbol_metas), data_tx))
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

    /// 启动 MarketData 分发任务
    fn spawn_market_data_dispatcher(
        mut data_rx: mpsc::Receiver<MarketData>,
        executors: Vec<ActorRef<ExecutorActor>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(data) = data_rx.recv().await {
                for executor in &executors {
                    let _ = executor.tell(data.clone()).await;
                }
            }
        })
    }

    /// 启动 Signal 转发任务
    fn spawn_signal_forwarder(
        mut signal_rx: mpsc::Receiver<Signal>,
        processor: ActorRef<SignalProcessorActor>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(signal) = signal_rx.recv().await {
                let _ = processor.tell(signal).await;
            }
        })
    }
}

impl Actor for EngineActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "EngineActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.downgrade());
        tracing::info!("EngineActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // 停止后台任务
        if let Some(task) = self.dispatcher_task.take() {
            task.abort();
        }
        if let Some(task) = self.forwarder_task.take() {
            task.abort();
        }

        // 停止所有 ExchangeActors (无 match 分发)
        for (_, actor) in &self.exchange_actors {
            actor.stop().await;
        }

        // 停止其他 Actors
        for executor in &self.executor_actors {
            executor.stop_gracefully().await.ok();
        }
        if let Some(ref processor) = self.signal_processor {
            processor.stop_gracefully().await.ok();
        }
        if let Some(ref clock) = self.clock {
            clock.stop_gracefully().await.ok();
        }

        tracing::info!("EngineActor stopped");
        Ok(())
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

        // 3. 创建数据聚合 channel
        let (data_tx, data_rx) = mpsc::channel::<MarketData>(1024);

        // 4. 创建 ExchangeActors (无 match 分发)
        for exchange in &required_exchanges {
            let actor = self.create_exchange_actor(*exchange, data_tx.clone())?;
            self.exchange_actors.insert(*exchange, actor);
        }

        // 5. 发送订阅请求 (无 match 分发)
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

        // 6. 创建 Signal channel
        let (signal_tx, signal_rx) = mpsc::channel::<Signal>(256);

        // 7. 创建 SignalProcessorActor
        let signal_processor = kameo::spawn(SignalProcessorActor::new(SignalProcessorArgs {
            executors: self.rests.clone(),
        }));
        self.signal_processor = Some(signal_processor.clone());

        // 启动 signal 转发任务
        self.forwarder_task = Some(Self::spawn_signal_forwarder(signal_rx, signal_processor));

        // 8. 创建 ExecutorActors
        let symbol_metas = Arc::new(self.symbol_metas.clone());
        let strategies = std::mem::take(&mut self.strategies);

        for strategy in strategies {
            let executor = kameo::spawn(ExecutorActor::new(ExecutorArgs {
                strategy,
                symbol_metas: symbol_metas.clone(),
                signal_tx: signal_tx.clone(),
            }));
            self.executor_actors.push(executor);
        }

        // 9. 创建 ClockActor
        let binance_executor = self.rests.get(&Exchange::Binance).cloned();
        let clock = kameo::spawn(ClockActor::new(ClockArgs {
            interval_ms: 1000,
            binance_executor,
            data_tx: data_tx.clone(),
        }));
        self.clock = Some(clock.clone());

        // 注册所有 Executor 到 Clock
        for executor in &self.executor_actors {
            let _ = clock
                .tell(RegisterExecutor {
                    executor: executor.clone(),
                })
                .await;
        }

        // 10. 启动 MarketData 分发任务
        self.dispatcher_task =
            Some(Self::spawn_market_data_dispatcher(data_rx, self.executor_actors.clone()));

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

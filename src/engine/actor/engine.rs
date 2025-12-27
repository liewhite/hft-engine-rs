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
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::request::MessageSend;
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

/// EngineActor 初始化参数
pub struct EngineActorArgs {
    pub exchanges_config: ExchangesConfig,
}

/// EngineActor - 顶层管理 Actor
pub struct EngineActor {
    /// 交易所配置
    exchanges_config: ExchangesConfig,
    /// 策略列表
    strategies: Vec<Box<dyn Strategy>>,
    /// Symbol 元数据
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,
    /// REST 客户端
    rests: HashMap<Exchange, Arc<dyn ExchangeExecutor>>,

    /// Binance ExchangeActor
    binance_actor: Option<ActorRef<ExchangeActor<BinanceConfig>>>,
    /// OKX ExchangeActor
    okx_actor: Option<ActorRef<ExchangeActor<OkxConfig>>>,
    /// ExecutorActors
    executor_actors: Vec<ActorRef<ExecutorActor>>,
    /// SignalProcessorActor
    signal_processor: Option<ActorRef<SignalProcessorActor>>,
    /// ClockActor
    clock: Option<ActorRef<ClockActor>>,

    /// Actor 自身引用
    self_ref: Option<WeakActorRef<Self>>,
}

impl EngineActor {
    pub fn new(args: EngineActorArgs) -> Self {
        Self {
            exchanges_config: args.exchanges_config,
            strategies: Vec::new(),
            symbol_metas: HashMap::new(),
            rests: HashMap::new(),
            binance_actor: None,
            okx_actor: None,
            executor_actors: Vec::new(),
            signal_processor: None,
            clock: None,
            self_ref: None,
        }
    }

    /// 初始化 REST 客户端
    fn init_rest_clients(
        &mut self,
        exchanges: &HashSet<Exchange>,
    ) -> Result<(), ExchangeError> {
        for exchange in exchanges {
            if self.rests.contains_key(exchange) {
                continue;
            }

            match exchange {
                Exchange::Binance => {
                    let rest: Arc<dyn ExchangeExecutor> = Arc::new(BinanceRestClient::new(
                        self.exchanges_config.binance.api_key.clone(),
                        self.exchanges_config.binance.secret.clone(),
                    )?);
                    self.rests.insert(Exchange::Binance, rest);
                }
                Exchange::OKX => {
                    let rest: Arc<dyn ExchangeExecutor> = Arc::new(OkxRestClient::new(
                        self.exchanges_config.okx.api_key.clone(),
                        self.exchanges_config.okx.secret.clone(),
                        self.exchanges_config.okx.passphrase.clone(),
                    )?);
                    self.rests.insert(Exchange::OKX, rest);
                }
            }

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

    /// 创建 Binance ExchangeActor
    fn create_binance_actor(
        &self,
        data_tx: mpsc::Sender<MarketData>,
    ) -> ActorRef<ExchangeActor<BinanceConfig>> {
        let symbol_metas: HashMap<Symbol, SymbolMeta> = self
            .symbol_metas
            .iter()
            .filter(|((e, _), _)| *e == Exchange::Binance)
            .map(|((_, s), m)| (s.clone(), m.clone()))
            .collect();

        let credentials = BinanceCredentials {
            api_key: self.exchanges_config.binance.api_key.clone(),
            secret: self.exchanges_config.binance.secret.clone(),
            listen_key: None,
        };

        kameo::spawn(ExchangeActor::<BinanceConfig>::new(ExchangeActorArgs {
            symbol_metas: Arc::new(symbol_metas),
            credentials,
            data_sink: data_tx,
        }))
    }

    /// 创建 OKX ExchangeActor
    fn create_okx_actor(
        &self,
        data_tx: mpsc::Sender<MarketData>,
    ) -> ActorRef<ExchangeActor<OkxConfig>> {
        let symbol_metas: HashMap<Symbol, SymbolMeta> = self
            .symbol_metas
            .iter()
            .filter(|((e, _), _)| *e == Exchange::OKX)
            .map(|((_, s), m)| (s.clone(), m.clone()))
            .collect();

        let credentials = OkxCredentials {
            api_key: self.exchanges_config.okx.api_key.clone(),
            secret: self.exchanges_config.okx.secret.clone(),
            passphrase: self.exchanges_config.okx.passphrase.clone(),
        };

        kameo::spawn(ExchangeActor::<OkxConfig>::new(ExchangeActorArgs {
            symbol_metas: Arc::new(symbol_metas),
            credentials,
            data_sink: data_tx,
        }))
    }

    /// 发送订阅请求
    async fn send_subscriptions<C: ExchangeConfig>(
        actor: &ActorRef<ExchangeActor<C>>,
        symbol_streams: &HashMap<Symbol, HashSet<PublicDataType>>,
    ) {
        for (symbol, data_types) in symbol_streams {
            if data_types.contains(&PublicDataType::FundingRate) {
                let _ = actor
                    .ask(Subscribe {
                        kind: SubscriptionKind::FundingRate {
                            symbol: symbol.clone(),
                        },
                    })
                    .send()
                    .await;
            }
            if data_types.contains(&PublicDataType::BBO) {
                let _ = actor
                    .ask(Subscribe {
                        kind: SubscriptionKind::BBO {
                            symbol: symbol.clone(),
                        },
                    })
                    .send()
                    .await;
            }
        }

        // 订阅 private 数据
        let _ = actor
            .ask(Subscribe {
                kind: SubscriptionKind::Private,
            })
            .send()
            .await;
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
    ) {
        tokio::spawn(async move {
            while let Some(data) = data_rx.recv().await {
                for executor in &executors {
                    let _ = executor.tell(data.clone()).await;
                }
            }
        });
    }

    /// 启动 Signal 转发任务
    fn spawn_signal_forwarder(
        mut signal_rx: mpsc::Receiver<Signal>,
        processor: ActorRef<SignalProcessorActor>,
    ) {
        tokio::spawn(async move {
            while let Some(signal) = signal_rx.recv().await {
                let _ = processor.tell(signal).await;
            }
        });
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
        // 停止所有子 Actor
        if let Some(ref actor) = self.binance_actor {
            actor.stop_gracefully().await.ok();
        }
        if let Some(ref actor) = self.okx_actor {
            actor.stop_gracefully().await.ok();
        }
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

        // 4. 创建 ExchangeActors
        if required_exchanges.contains(&Exchange::Binance) {
            self.binance_actor = Some(self.create_binance_actor(data_tx.clone()));
        }

        if required_exchanges.contains(&Exchange::OKX) {
            self.okx_actor = Some(self.create_okx_actor(data_tx.clone()));
        }

        // 5. 发送订阅请求
        for (exchange, symbol_streams) in &aggregated {
            match exchange {
                Exchange::Binance => {
                    if let Some(ref actor) = self.binance_actor {
                        Self::send_subscriptions(actor, symbol_streams).await;
                    }
                }
                Exchange::OKX => {
                    if let Some(ref actor) = self.okx_actor {
                        Self::send_subscriptions(actor, symbol_streams).await;
                    }
                }
            }

            tracing::info!(
                exchange = %exchange,
                symbols = symbol_streams.len(),
                "Subscriptions sent"
            );
        }

        // 6. 创建 Signal channel
        let (signal_tx, signal_rx) = mpsc::channel::<Signal>(256);

        // 7. 创建 SignalProcessorActor
        let signal_processor = kameo::spawn(SignalProcessorActor::new(SignalProcessorArgs {
            executors: self.rests.clone(),
        }));
        self.signal_processor = Some(signal_processor.clone());

        // 启动 signal 转发任务
        Self::spawn_signal_forwarder(signal_rx, signal_processor);

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
        Self::spawn_market_data_dispatcher(data_rx, self.executor_actors.clone());

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

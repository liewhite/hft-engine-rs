//! ActorEngine - 基于 Actor 模式的引擎实现
//!
//! 使用 kameo Actor 框架管理所有组件

use super::actor::{
    ClockActor, ClockArgs, ExecutorActor, ExecutorArgs, RegisterExecutor, SignalProcessorActor,
    SignalProcessorArgs,
};
use crate::config::ExchangesConfig;
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{BinanceConfig, BinanceCredentials, BinanceRestClient};
use crate::exchange::okx::{OkxConfig, OkxCredentials, OkxRestClient};
use crate::exchange::subscriber::{
    MarketData, Subscribe, SubscriberActor, SubscriberArgs, SubscriptionKind,
};
use crate::exchange::ExchangeExecutor;
use crate::exchange::PublicDataType;
use crate::strategy::{PublicStreams, Signal, Strategy};
use kameo::actor::ActorRef;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

/// ActorEngine - 基于 Actor 的引擎
pub struct ActorEngine {
    exchanges_config: ExchangesConfig,
    strategies: Vec<Box<dyn Strategy>>,
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,
    rests: HashMap<Exchange, Arc<dyn ExchangeExecutor>>,
}

impl ActorEngine {
    /// 创建新的 ActorEngine
    pub fn new(exchanges_config: ExchangesConfig) -> Self {
        Self {
            exchanges_config,
            strategies: Vec::new(),
            symbol_metas: HashMap::new(),
            rests: HashMap::new(),
        }
    }

    /// 添加策略
    pub fn add_strategy<S: Strategy + 'static>(&mut self, strategy: S) {
        self.strategies.push(Box::new(strategy));
    }

    /// 启动引擎
    pub async fn run(&mut self) -> Result<(), ExchangeError> {
        // 1. 聚合所有策略需要的 public streams
        let aggregated = Self::aggregate_public_streams(&self.strategies);
        let required_exchanges: HashSet<Exchange> = aggregated.keys().cloned().collect();

        tracing::info!(
            exchanges = aggregated.len(),
            strategies = self.strategies.len(),
            "Starting ActorEngine"
        );

        // 2. 初始化 REST 客户端并获取 SymbolMeta
        self.init_rest_clients(&required_exchanges)?;
        self.fetch_symbol_metas(&aggregated).await?;

        // 3. 创建数据聚合 channel
        let (data_tx, data_rx) = mpsc::channel::<MarketData>(1024);

        // 4. Spawn SubscriberActors
        let binance_subscriber = if required_exchanges.contains(&Exchange::Binance) {
            Some(self.spawn_binance_subscriber(data_tx.clone()).await?)
        } else {
            None
        };

        let okx_subscriber = if required_exchanges.contains(&Exchange::OKX) {
            Some(self.spawn_okx_subscriber(data_tx.clone()).await?)
        } else {
            None
        };

        // 5. 发送订阅请求
        for (exchange, symbol_streams) in &aggregated {
            match exchange {
                Exchange::Binance => {
                    if let Some(ref subscriber) = binance_subscriber {
                        Self::send_subscriptions(subscriber, symbol_streams).await;
                    }
                }
                Exchange::OKX => {
                    if let Some(ref subscriber) = okx_subscriber {
                        Self::send_subscriptions(subscriber, symbol_streams).await;
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

        // 7. Spawn SignalProcessorActor
        let signal_processor = kameo::spawn(SignalProcessorActor::new(SignalProcessorArgs {
            executors: self.rests.clone(),
        }));

        // 启动 signal 转发任务
        Self::spawn_signal_forwarder(signal_rx, signal_processor);

        // 8. Spawn ExecutorActors
        let symbol_metas = Arc::new(self.symbol_metas.clone());
        let strategies = std::mem::take(&mut self.strategies);
        let mut executor_refs = Vec::new();

        for strategy in strategies {
            let executor = kameo::spawn(ExecutorActor::new(ExecutorArgs {
                strategy,
                symbol_metas: symbol_metas.clone(),
                signal_tx: signal_tx.clone(),
            }));
            executor_refs.push(executor);
        }

        // 9. Spawn ClockActor
        let binance_executor = self.rests.get(&Exchange::Binance).cloned();
        let clock = kameo::spawn(ClockActor::new(ClockArgs {
            interval_ms: 1000,
            binance_executor,
            data_tx: data_tx.clone(),
        }));

        // 注册所有 Executor 到 Clock
        for executor in &executor_refs {
            let _ = clock
                .tell(RegisterExecutor {
                    executor: executor.clone(),
                })
                .await;
        }

        // 10. 启动 MarketData 分发任务
        Self::spawn_market_data_dispatcher(data_rx, executor_refs);

        tracing::info!("ActorEngine started");
        Ok(())
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

    /// Spawn Binance SubscriberActor
    async fn spawn_binance_subscriber(
        &self,
        data_tx: mpsc::Sender<MarketData>,
    ) -> Result<ActorRef<SubscriberActor<BinanceConfig>>, ExchangeError> {
        let symbol_metas: HashMap<Symbol, SymbolMeta> = self
            .symbol_metas
            .iter()
            .filter(|((e, _), _)| *e == Exchange::Binance)
            .map(|((_, s), m)| (s.clone(), m.clone()))
            .collect();

        let credentials = BinanceCredentials {
            api_key: self.exchanges_config.binance.api_key.clone(),
            secret: self.exchanges_config.binance.secret.clone(),
            listen_key: None, // TODO: 从 REST 获取 ListenKey
        };

        let actor = kameo::spawn(SubscriberActor::<BinanceConfig>::new(SubscriberArgs {
            symbol_metas: Arc::new(symbol_metas),
            credentials,
            data_sink: data_tx,
        }));

        Ok(actor)
    }

    /// Spawn OKX SubscriberActor
    async fn spawn_okx_subscriber(
        &self,
        data_tx: mpsc::Sender<MarketData>,
    ) -> Result<ActorRef<SubscriberActor<OkxConfig>>, ExchangeError> {
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

        let actor = kameo::spawn(SubscriberActor::<OkxConfig>::new(SubscriberArgs {
            symbol_metas: Arc::new(symbol_metas),
            credentials,
            data_sink: data_tx,
        }));

        Ok(actor)
    }

    /// 发送订阅请求到 SubscriberActor
    async fn send_subscriptions<C>(
        subscriber: &ActorRef<SubscriberActor<C>>,
        symbol_streams: &HashMap<Symbol, HashSet<PublicDataType>>,
    ) where
        C: crate::exchange::subscriber::ExchangeConfig,
    {
        for (symbol, data_types) in symbol_streams {
            if data_types.contains(&PublicDataType::FundingRate) {
                let _ = subscriber
                    .ask(Subscribe {
                        kind: SubscriptionKind::FundingRate {
                            symbol: symbol.clone(),
                        },
                    })
                    .await;
            }
            if data_types.contains(&PublicDataType::BBO) {
                let _ = subscriber
                    .ask(Subscribe {
                        kind: SubscriptionKind::BBO {
                            symbol: symbol.clone(),
                        },
                    })
                    .await;
            }
        }

        // 订阅 private 数据
        let _ = subscriber
            .ask(Subscribe {
                kind: SubscriptionKind::Private,
            })
            .await;
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

    /// 等待退出信号
    pub async fn wait_for_shutdown(&self) {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl-c");
        tracing::info!("Received shutdown signal");
    }
}

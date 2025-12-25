use crate::config::{ExchangesConfig, MetricsConfig};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta, now_ms};
use crate::exchange::binance::{BinanceRestClient, BinanceWebSocket};
use crate::exchange::okx::{OkxRestClient, OkxWebSocket};
use crate::exchange::{ExchangeExecutor, ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::messaging::ExchangeEvent;
use crate::strategy::{PublicStreams, Signal, Strategy};
use super::executor::{Executor, SymbolMetas};
use super::metrics::MetricsManager;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// 资金费率套利引擎 - 管理多个策略的执行
pub struct FundingEngine {
    exchanges: HashMap<Exchange, Arc<dyn ExchangeWebSocket>>,
    executors: HashMap<Exchange, Arc<dyn ExchangeExecutor>>,
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,
    strategies: Vec<Box<dyn Strategy>>,
    metrics_config: MetricsConfig,
    cancel_token: CancellationToken,
}

impl FundingEngine {
    /// 创建引擎，自动注册所有支持的交易所
    pub fn new(
        exchanges_config: &ExchangesConfig,
        metrics_config: MetricsConfig,
    ) -> Result<Self, ExchangeError> {
        let mut exchanges: HashMap<Exchange, Arc<dyn ExchangeWebSocket>> = HashMap::new();
        let mut executors: HashMap<Exchange, Arc<dyn ExchangeExecutor>> = HashMap::new();

        // 注册 Binance
        let binance_ws: Arc<dyn ExchangeWebSocket> = Arc::new(BinanceWebSocket::new(
            exchanges_config.binance.api_key.clone(),
            exchanges_config.binance.secret.clone(),
        )?);
        let binance_rest: Arc<dyn ExchangeExecutor> = Arc::new(BinanceRestClient::new(
            exchanges_config.binance.api_key.clone(),
            exchanges_config.binance.secret.clone(),
        )?);
        exchanges.insert(Exchange::Binance, binance_ws);
        executors.insert(Exchange::Binance, binance_rest);

        // 注册 OKX
        let okx_ws: Arc<dyn ExchangeWebSocket> = Arc::new(OkxWebSocket::new(
            exchanges_config.okx.api_key.clone(),
            exchanges_config.okx.secret.clone(),
            exchanges_config.okx.passphrase.clone(),
        )?);
        let okx_rest: Arc<dyn ExchangeExecutor> = Arc::new(OkxRestClient::new(
            exchanges_config.okx.api_key.clone(),
            exchanges_config.okx.secret.clone(),
            exchanges_config.okx.passphrase.clone(),
        )?);
        exchanges.insert(Exchange::OKX, okx_ws);
        executors.insert(Exchange::OKX, okx_rest);

        Ok(Self {
            exchanges,
            executors,
            symbol_metas: HashMap::new(),
            strategies: Vec::new(),
            metrics_config,
            cancel_token: CancellationToken::new(),
        })
    }

    /// 添加策略
    pub fn add_strategy<S: Strategy + 'static>(&mut self, strategy: S) {
        self.strategies.push(Box::new(strategy));
    }

    /// 启动引擎
    pub async fn run(&mut self) -> Result<(), ExchangeError> {
        const SINK_CAPACITY: usize = 256;
        let token = self.cancel_token.clone();

        // 1. 聚合所有策略需要的 public streams
        let aggregated = Self::aggregate_public_streams(&self.strategies);

        tracing::info!(
            exchanges = aggregated.len(),
            strategies = self.strategies.len(),
            "Starting engine"
        );

        // 2. 获取并缓存 SymbolMeta
        for (exchange, symbol_streams) in &aggregated {
            let executor = self.executors.get(exchange).ok_or_else(|| {
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

        // 3. 为每个交易所创建 sinks 并连接
        let mut public_sinks: Vec<(Exchange, PublicSinks)> = Vec::new();
        let mut private_sinks: Vec<(Exchange, PrivateSinks)> = Vec::new();

        for (exchange, symbol_streams) in &aggregated {
            let ws = self.exchanges.get(exchange).ok_or_else(|| {
                ExchangeError::Other(format!("Exchange {:?} not registered", exchange))
            })?;

            let symbols_vec: Vec<Symbol> = symbol_streams.keys().cloned().collect();

            let pub_sinks = PublicSinks::from_streams(symbol_streams, SINK_CAPACITY);
            let priv_sinks = PrivateSinks::new(&symbols_vec, SINK_CAPACITY);

            // 连接 WebSocket
            ws.connect_public(pub_sinks.clone(), token.clone()).await?;
            ws.connect_private(priv_sinks.clone(), token.clone()).await?;

            public_sinks.push((*exchange, pub_sinks));
            private_sinks.push((*exchange, priv_sinks));

            tracing::info!(exchange = %exchange, symbols = symbols_vec.len(), "Exchange connected");
        }

        // 4. 启动 Metrics
        self.start_metrics(&public_sinks, &private_sinks, token.clone());

        // 5. 创建 SignalQueue
        let (signal_tx, mut signal_rx) = mpsc::channel::<Signal>(256);

        // 6. 创建 Clock broadcast 并启动定时推送
        let (clock_tx, _) = broadcast::channel::<ExchangeEvent>(16);
        let clock_tx_clone = clock_tx.clone();
        let clock_token = token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = clock_token.cancelled() => break,
                    _ = interval.tick() => {
                        let _ = clock_tx_clone.send(ExchangeEvent::Clock { timestamp: now_ms() });
                    }
                }
            }
        });

        // 7. 为每个策略创建 Executor 并启动
        let symbol_metas: SymbolMetas = Arc::new(self.symbol_metas.clone());
        let strategies = std::mem::take(&mut self.strategies);
        for strategy in strategies {
            let executor = Executor::new(strategy);
            executor.run(
                &public_sinks,
                &private_sinks,
                symbol_metas.clone(),
                clock_tx.subscribe(),
                signal_tx.clone(),
                token.clone(),
            );
        }

        // 8. 处理信号
        tokio::spawn(async move {
            while let Some(signal) = signal_rx.recv().await {
                match signal {
                    Signal::PlaceOrder(order) => {
                        tracing::info!(
                            exchange = %order.exchange,
                            symbol = %order.symbol,
                            side = %order.side,
                            quantity = %order.quantity,
                            "Signal received: PlaceOrder"
                        );
                        // TODO: 执行下单逻辑
                    }
                }
            }
        });

        tracing::info!("Engine started");

        Ok(())
    }

    /// 聚合所有策略需要的 public streams
    ///
    /// 将多个策略的 PublicStreams 合并为一个，同一 (exchange, symbol) 的 data_types 取并集
    fn aggregate_public_streams(strategies: &[Box<dyn Strategy>]) -> PublicStreams {
        let mut aggregated: PublicStreams = HashMap::new();

        for strategy in strategies {
            for (exchange, symbol_streams) in strategy.public_streams() {
                let exchange_entry = aggregated.entry(exchange).or_default();
                for (symbol, data_types) in symbol_streams {
                    exchange_entry
                        .entry(symbol)
                        .or_default()
                        .extend(data_types);
                }
            }
        }

        aggregated
    }

    /// 启动 Metrics 订阅和 push
    fn start_metrics(
        &self,
        public_sinks: &[(Exchange, PublicSinks)],
        private_sinks: &[(Exchange, PrivateSinks)],
        cancel_token: CancellationToken,
    ) {
        let metrics = MetricsManager::new();
        metrics.start(
            &self.metrics_config,
            public_sinks,
            private_sinks,
            cancel_token,
        );
    }

    /// 停止引擎
    pub fn stop(&self) {
        tracing::info!("Stopping engine...");
        self.cancel_token.cancel();
        tracing::info!("Engine stopped");
    }

    /// 等待退出信号
    pub async fn wait_for_shutdown(&self) {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl-c");
        tracing::info!("Received shutdown signal");
    }
}


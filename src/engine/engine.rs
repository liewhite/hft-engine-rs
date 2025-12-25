use super::executor::{Executor, SymbolMetas};
use crate::config::ExchangesConfig;
use crate::domain::{now_ms, Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{BinanceRestClient, BinanceWebSocket};
use crate::exchange::okx::{OkxRestClient, OkxWebSocket};
use crate::exchange::{ExchangeExecutor, ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::messaging::ExchangeEvent;
use crate::strategy::{PublicStreams, Signal, Strategy};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// 资金费率套利引擎 - 管理多个策略的执行
pub struct Engine {
    exchanges_config: ExchangesConfig,
    wss: HashMap<Exchange, Arc<dyn ExchangeWebSocket>>,
    rests: HashMap<Exchange, Arc<dyn ExchangeExecutor>>,
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,
    strategies: Vec<Box<dyn Strategy>>,
    cancel_token: CancellationToken,
}

impl Engine {
    /// 创建引擎，不初始化交易所客户端
    pub fn new(exchanges_config: ExchangesConfig) -> Self {
        Self {
            exchanges_config,
            wss: HashMap::new(),
            rests: HashMap::new(),
            symbol_metas: HashMap::new(),
            strategies: Vec::new(),
            cancel_token: CancellationToken::new(),
        }
    }

    /// 按需初始化交易所客户端
    fn init_exchanges(&mut self, exchanges: &HashSet<Exchange>) -> Result<(), ExchangeError> {
        for exchange in exchanges {
            if self.wss.contains_key(exchange) {
                continue;
            }

            match exchange {
                Exchange::Binance => {
                    let ws: Arc<dyn ExchangeWebSocket> = Arc::new(BinanceWebSocket::new(
                        self.exchanges_config.binance.api_key.clone(),
                        self.exchanges_config.binance.secret.clone(),
                    )?);
                    let rest: Arc<dyn ExchangeExecutor> = Arc::new(BinanceRestClient::new(
                        self.exchanges_config.binance.api_key.clone(),
                        self.exchanges_config.binance.secret.clone(),
                    )?);
                    self.wss.insert(Exchange::Binance, ws);
                    self.rests.insert(Exchange::Binance, rest);
                }
                Exchange::OKX => {
                    let ws: Arc<dyn ExchangeWebSocket> = Arc::new(OkxWebSocket::new(
                        self.exchanges_config.okx.api_key.clone(),
                        self.exchanges_config.okx.secret.clone(),
                        self.exchanges_config.okx.passphrase.clone(),
                    )?);
                    let rest: Arc<dyn ExchangeExecutor> = Arc::new(OkxRestClient::new(
                        self.exchanges_config.okx.api_key.clone(),
                        self.exchanges_config.okx.secret.clone(),
                        self.exchanges_config.okx.passphrase.clone(),
                    )?);
                    self.wss.insert(Exchange::OKX, ws);
                    self.rests.insert(Exchange::OKX, rest);
                }
            }

            tracing::info!(exchange = %exchange, "Exchange client initialized");
        }

        Ok(())
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

        // 2. 根据策略需求初始化交易所客户端
        let required_exchanges: HashSet<Exchange> = aggregated.keys().cloned().collect();
        self.init_exchanges(&required_exchanges)?;

        tracing::info!(
            exchanges = aggregated.len(),
            strategies = self.strategies.len(),
            "Starting engine"
        );

        // 3. 获取并缓存 SymbolMeta
        for (exchange, symbol_streams) in &aggregated {
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
        // 4. 为每个交易所创建 sinks 并连接
        let mut public_sinks: Vec<(Exchange, PublicSinks)> = Vec::new();
        let mut private_sinks: Vec<(Exchange, PrivateSinks)> = Vec::new();

        for (exchange, symbol_streams) in &aggregated {
            let ws = self.wss.get(exchange).ok_or_else(|| {
                ExchangeError::Other(format!("Exchange {:?} not registered", exchange))
            })?;

            let symbols_vec: Vec<Symbol> = symbol_streams.keys().cloned().collect();

            let pub_sinks = PublicSinks::from_streams(symbol_streams, SINK_CAPACITY);
            let priv_sinks = PrivateSinks::new(&symbols_vec, SINK_CAPACITY);

            // 连接 WebSocket
            ws.connect_public(pub_sinks.clone(), token.clone()).await?;
            ws.connect_private(priv_sinks.clone(), token.clone())
                .await?;

            public_sinks.push((*exchange, pub_sinks));
            private_sinks.push((*exchange, priv_sinks));

            tracing::info!(exchange = %exchange, symbols = symbols_vec.len(), "Exchange connected");
        }

        // 5. 创建 SignalQueue
        let (signal_tx, mut signal_rx) = mpsc::channel::<Signal>(256);

        // 6. 创建 Clock broadcast 并启动定时推送
        let (clock_tx, _) = broadcast::channel::<ExchangeEvent>(16);
        let clock_tx_clone = clock_tx.clone();
        let clock_token = token.clone();
        // 只有 Binance 需要通过 REST 查询 equity (OKX 通过 WebSocket 推送)
        let binance_executor = self.rests.get(&Exchange::Binance).cloned();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = clock_token.cancelled() => break,
                    _ = interval.tick() => {
                        let timestamp = now_ms();
                        // 查询 Binance equity 并广播
                        if let Some(ref executor) = binance_executor {
                            match executor.fetch_equity().await {
                                Ok(equity) => {
                                    let _ = clock_tx_clone.send(ExchangeEvent::EquityUpdate {
                                        exchange: Exchange::Binance,
                                        equity,
                                        timestamp,
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        exchange = %Exchange::Binance,
                                        error = %e,
                                        "Failed to fetch equity"
                                    );
                                }
                            }
                        }
                        // 最后发送 Clock 事件
                        let _ = clock_tx_clone.send(ExchangeEvent::Clock { timestamp });
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
        let signal_executors = self.rests.clone();
        tokio::spawn(async move {
            while let Some(signal) = signal_rx.recv().await {
                match signal {
                    Signal::PlaceOrder(order) => {
                        let executor = match signal_executors.get(&order.exchange) {
                            Some(e) => e.clone(),
                            None => {
                                tracing::error!(
                                    exchange = %order.exchange,
                                    "No executor found for exchange"
                                );
                                continue;
                            }
                        };

                        tracing::info!(
                            exchange = %order.exchange,
                            symbol = %order.symbol,
                            side = %order.side,
                            order_type = ?order.order_type,
                            quantity = order.quantity,
                            client_order_id = ?order.client_order_id,
                            "Placing order"
                        );

                        match executor.place_order(order.clone()).await {
                            Ok(order_id) => {
                                tracing::info!(
                                    exchange = %order.exchange,
                                    symbol = %order.symbol,
                                    order_id = %order_id,
                                    client_order_id = ?order.client_order_id,
                                    "Order placed successfully"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    exchange = %order.exchange,
                                    symbol = %order.symbol,
                                    client_order_id = ?order.client_order_id,
                                    error = %e,
                                    "Failed to place order"
                                );
                            }
                        }
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
                    exchange_entry.entry(symbol).or_default().extend(data_types);
                }
            }
        }

        aggregated
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

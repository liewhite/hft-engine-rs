use crate::domain::{Exchange, ExchangeError, Symbol};
use crate::engine::executor::Executor;
use crate::exchange::{ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::messaging::ExchangeEvent;
use crate::strategy::{MarketDataType, Signal, Strategy};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 引擎 - 管理多个策略的执行
pub struct Engine {
    exchanges: HashMap<Exchange, Arc<dyn ExchangeWebSocket>>,
    strategies: Vec<Box<dyn Strategy>>,
    cancel_token: CancellationToken,
}

impl Engine {
    pub fn new(exchanges: Vec<Arc<dyn ExchangeWebSocket>>) -> Self {
        let exchanges_map: HashMap<_, _> = exchanges
            .into_iter()
            .map(|ws| (ws.exchange(), ws))
            .collect();

        Self {
            exchanges: exchanges_map,
            strategies: Vec::new(),
            cancel_token: CancellationToken::new(),
        }
    }

    /// 添加策略
    pub fn add_strategy<S: Strategy + 'static>(&mut self, strategy: S) {
        self.strategies.push(Box::new(strategy));
    }

    /// 启动引擎
    pub async fn run(&mut self) -> Result<(), ExchangeError> {
        const SINK_CAPACITY: usize = 256;
        let token = self.cancel_token.clone();

        // 1. 收集所有策略需要的 (exchange, symbols)
        let mut required: HashMap<Exchange, HashSet<Symbol>> = HashMap::new();
        for strategy in &self.strategies {
            for exchange in strategy.exchanges() {
                for symbol in strategy.symbols() {
                    required
                        .entry(exchange)
                        .or_default()
                        .insert(symbol);
                }
            }
        }

        tracing::info!(
            exchanges = required.len(),
            strategies = self.strategies.len(),
            "Starting engine"
        );

        // 2. 为每个交易所创建 sinks 并连接
        let mut public_sinks: Vec<(Exchange, PublicSinks)> = Vec::new();
        let mut private_sinks: Vec<(Exchange, PrivateSinks)> = Vec::new();

        for (exchange, symbols) in &required {
            let ws = self.exchanges.get(exchange).ok_or_else(|| {
                ExchangeError::Other(format!("Exchange {:?} not registered", exchange))
            })?;

            let symbols_vec: Vec<Symbol> = symbols.iter().cloned().collect();

            let pub_sinks = PublicSinks::new(&symbols_vec, SINK_CAPACITY);
            let priv_sinks = PrivateSinks::new(&symbols_vec, SINK_CAPACITY);

            // 连接 WebSocket
            ws.connect_public(pub_sinks.clone(), token.clone()).await?;
            ws.connect_private(priv_sinks.clone(), token.clone()).await?;

            public_sinks.push((*exchange, pub_sinks));
            private_sinks.push((*exchange, priv_sinks));

            tracing::info!(exchange = %exchange, symbols = symbols.len(), "Exchange connected");
        }

        // 3. 创建 SignalQueue
        let (signal_tx, mut signal_rx) = mpsc::channel::<Signal>(256);

        // 4. 为每个策略创建 Executor 并启动
        // 注意：这里需要 take ownership of strategies
        let strategies = std::mem::take(&mut self.strategies);
        for strategy in strategies {
            // 由于 Strategy 是 trait object，需要特殊处理
            // 这里我们用 StrategyWrapper 来包装
            let wrapper = StrategyWrapper(strategy);
            let executor = Executor::new(wrapper);
            executor.run(&public_sinks, &private_sinks, signal_tx.clone(), token.clone());
        }

        // 5. 处理信号
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


/// 包装 Box<dyn Strategy> 以实现 Strategy trait
struct StrategyWrapper(Box<dyn Strategy>);

impl Strategy for StrategyWrapper {
    fn exchanges(&self) -> Vec<Exchange> {
        self.0.exchanges()
    }

    fn symbols(&self) -> Vec<Symbol> {
        self.0.symbols()
    }

    fn market_data_types(&self) -> Vec<MarketDataType> {
        self.0.market_data_types()
    }

    fn on_event(&mut self, event: ExchangeEvent) -> Vec<Signal> {
        self.0.on_event(event)
    }
}

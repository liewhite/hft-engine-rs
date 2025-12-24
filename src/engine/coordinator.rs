use crate::domain::{ExchangeError, Symbol};
use crate::exchange::{ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::messaging::{EventDispatcher, SymbolEventBus};
use crate::strategy::Strategy;
use crate::engine::processor::SymbolProcessor;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 系统协调器 - 管理所有组件的生命周期
pub struct Coordinator<S>
where
    S: Strategy + Clone,
{
    exchanges: Vec<Arc<dyn ExchangeWebSocket>>,
    strategy: Arc<S>,
    symbols: Vec<Symbol>,
    cancel_token: CancellationToken,
    event_bus: Option<Arc<SymbolEventBus>>,
}

impl<S> Coordinator<S>
where
    S: Strategy + Clone + Send + Sync + 'static,
{
    pub fn new(
        exchanges: Vec<Arc<dyn ExchangeWebSocket>>,
        strategy: S,
        symbols: Vec<Symbol>,
    ) -> Self {
        Self {
            exchanges,
            strategy: Arc::new(strategy),
            symbols,
            cancel_token: CancellationToken::new(),
            event_bus: None,
        }
    }

    /// 启动系统
    pub async fn start(&mut self) -> Result<(), ExchangeError> {
        let token = self.cancel_token.clone();
        const SINK_CAPACITY: usize = 256;

        tracing::info!("Starting coordinator...");

        // 1. 创建 SymbolEventBus
        let (event_bus, receivers) = SymbolEventBus::new(&self.symbols, SINK_CAPACITY);
        self.event_bus = Some(event_bus.clone());

        // 2. 为每个交易所创建 sinks 并启动连接
        tracing::info!(
            exchanges = self.exchanges.len(),
            "Connecting to exchanges..."
        );

        for exchange_ws in &self.exchanges {
            let exchange = exchange_ws.exchange();

            // 创建 per-exchange Sinks
            let public_sinks = PublicSinks::new(&self.symbols, SINK_CAPACITY);
            let private_sinks = PrivateSinks::new(&self.symbols, SINK_CAPACITY);

            // 启动事件分发器 (必须在 WebSocket 连接之前启动)
            EventDispatcher::dispatch_public(
                exchange,
                &public_sinks,
                event_bus.clone(),
                token.clone(),
            );
            EventDispatcher::dispatch_private(
                exchange,
                &private_sinks,
                event_bus.clone(),
                token.clone(),
            );

            // 连接交易所 WebSocket
            exchange_ws
                .connect_public(public_sinks, token.clone())
                .await?;
            exchange_ws
                .connect_private(private_sinks, token.clone())
                .await?;

            tracing::info!(exchange = %exchange, "Exchange connected");
        }

        // 3. 启动 per-symbol 处理器
        tracing::info!("Starting symbol processors...");
        for (symbol, rx) in receivers {
            let processor = SymbolProcessor::new(symbol.clone(), self.strategy.clone());
            processor.run(rx, token.clone());
        }

        tracing::info!(
            symbols = self.symbols.len(),
            exchanges = self.exchanges.len(),
            "Coordinator started successfully"
        );

        Ok(())
    }

    /// 停止系统
    pub fn stop(&self) {
        tracing::info!("Stopping coordinator...");
        self.cancel_token.cancel();
        tracing::info!("Coordinator stopped");
    }

    /// 获取事件总线统计
    pub fn event_bus_stats(&self) -> Option<HashMap<Symbol, crate::messaging::QueueStats>> {
        self.event_bus.as_ref().map(|bus| bus.stats())
    }

    /// 等待系统退出信号
    pub async fn wait_for_shutdown(&self) {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl-c");
        tracing::info!("Received shutdown signal");
    }
}

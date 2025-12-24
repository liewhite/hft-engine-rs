use crate::domain::{Exchange, ExchangeError, Symbol};
use crate::exchange::{ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::messaging::{EventDispatcher, SymbolEventBus};
use crate::strategy::Strategy;
use crate::engine::processor::SymbolProcessor;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 系统协调器 - 管理所有组件的生命周期
pub struct Coordinator<B, O, S>
where
    B: ExchangeWebSocket,
    O: ExchangeWebSocket,
    S: Strategy + Clone,
{
    binance: Arc<B>,
    okx: Arc<O>,
    strategy: Arc<S>,
    symbols: Vec<Symbol>,
    cancel_token: CancellationToken,
    event_bus: Option<Arc<SymbolEventBus>>,
}

impl<B, O, S> Coordinator<B, O, S>
where
    B: ExchangeWebSocket + 'static,
    O: ExchangeWebSocket + 'static,
    S: Strategy + Clone + Send + Sync + 'static,
{
    pub fn new(binance: Arc<B>, okx: Arc<O>, strategy: S, symbols: Vec<Symbol>) -> Self {
        Self {
            binance,
            okx,
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

        // 2. 创建 per-exchange Sinks (消费者创建 sender)
        let binance_public_sinks = PublicSinks::new(&self.symbols, SINK_CAPACITY);
        let binance_private_sinks = PrivateSinks::new(&self.symbols, SINK_CAPACITY);
        let okx_public_sinks = PublicSinks::new(&self.symbols, SINK_CAPACITY);
        let okx_private_sinks = PrivateSinks::new(&self.symbols, SINK_CAPACITY);

        // 3. 启动事件分发器 (从 sinks 订阅，转发到 EventBus)
        //    必须在 WebSocket 连接之前启动，避免丢失初始消息
        tracing::info!("Starting event dispatchers...");
        EventDispatcher::dispatch_public(
            Exchange::Binance,
            &binance_public_sinks,
            event_bus.clone(),
            token.clone(),
        );
        EventDispatcher::dispatch_public(
            Exchange::OKX,
            &okx_public_sinks,
            event_bus.clone(),
            token.clone(),
        );
        EventDispatcher::dispatch_private(
            Exchange::Binance,
            &binance_private_sinks,
            event_bus.clone(),
            token.clone(),
        );
        EventDispatcher::dispatch_private(
            Exchange::OKX,
            &okx_private_sinks,
            event_bus.clone(),
            token.clone(),
        );

        // 4. 连接交易所 WebSocket (传入 sinks，WebSocket 只负责推送数据)
        tracing::info!("Connecting to exchanges...");
        self.binance
            .connect_public(binance_public_sinks, token.clone())
            .await?;
        self.binance
            .connect_private(binance_private_sinks, token.clone())
            .await?;
        self.okx
            .connect_public(okx_public_sinks, token.clone())
            .await?;
        self.okx
            .connect_private(okx_private_sinks, token.clone())
            .await?;

        // 5. 启动 per-symbol 处理器
        tracing::info!("Starting symbol processors...");
        for (symbol, rx) in receivers {
            let processor = SymbolProcessor::new(symbol.clone(), self.strategy.clone());
            processor.run(rx, token.clone());
        }

        tracing::info!(
            symbols = self.symbols.len(),
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

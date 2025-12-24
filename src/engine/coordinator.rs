use crate::domain::{Exchange, ExchangeError, Symbol};
use crate::exchange::ExchangeWebSocket;
use crate::messaging::{EventDispatcher, SymbolEventBus};
use crate::strategy::Strategy;
use crate::engine::processor::SymbolProcessor;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 生命周期管理
struct ComponentLifecycle {
    token: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}

impl ComponentLifecycle {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            handles: Vec::new(),
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    fn register(&mut self, handle: JoinHandle<()>) {
        self.handles.push(handle);
    }

    fn register_all(&mut self, handles: Vec<JoinHandle<()>>) {
        self.handles.extend(handles);
    }

    async fn shutdown(&self) {
        self.token.cancel();
        for handle in &self.handles {
            handle.abort();
        }
    }
}

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
    lifecycle: ComponentLifecycle,
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
            lifecycle: ComponentLifecycle::new(),
            event_bus: None,
        }
    }

    /// 启动系统
    pub async fn start(&mut self) -> Result<(), ExchangeError> {
        let token = self.lifecycle.token();

        tracing::info!("Starting coordinator...");

        // 1. 创建 SymbolEventBus
        let (event_bus, receivers) = SymbolEventBus::new(&self.symbols, 256);
        self.event_bus = Some(event_bus.clone());

        // 2. 连接交易所 Public WebSocket
        tracing::info!("Connecting to exchanges...");
        let binance_public = self
            .binance
            .connect_public(&self.symbols, token.clone())
            .await?;
        let okx_public = self
            .okx
            .connect_public(&self.symbols, token.clone())
            .await?;

        // 3. 连接交易所 Private WebSocket
        let binance_private = self.binance.connect_private(token.clone()).await?;
        let okx_private = self.okx.connect_private(token.clone()).await?;

        // 4. 启动事件分发器
        tracing::info!("Starting event dispatchers...");
        let binance_public_handles =
            EventDispatcher::dispatch_public(Exchange::Binance, &binance_public, event_bus.clone(), token.clone());
        let okx_public_handles =
            EventDispatcher::dispatch_public(Exchange::OKX, &okx_public, event_bus.clone(), token.clone());
        let binance_private_handles =
            EventDispatcher::dispatch_private(Exchange::Binance, &binance_private, event_bus.clone(), token.clone());
        let okx_private_handles =
            EventDispatcher::dispatch_private(Exchange::OKX, &okx_private, event_bus.clone(), token.clone());

        self.lifecycle.register_all(binance_public_handles);
        self.lifecycle.register_all(okx_public_handles);
        self.lifecycle.register_all(binance_private_handles);
        self.lifecycle.register_all(okx_private_handles);

        // 5. 启动 per-symbol 处理器
        tracing::info!("Starting symbol processors...");
        for (symbol, rx) in receivers {
            let processor = SymbolProcessor::new(symbol.clone(), self.strategy.clone());
            let handle = processor.run(rx, token.clone());
            self.lifecycle.register(handle);
        }

        tracing::info!(
            symbols = self.symbols.len(),
            "Coordinator started successfully"
        );

        Ok(())
    }

    /// 停止系统
    pub async fn stop(&self) {
        tracing::info!("Stopping coordinator...");
        self.lifecycle.shutdown().await;
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

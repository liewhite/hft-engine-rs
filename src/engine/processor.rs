use crate::domain::Symbol;
use crate::messaging::{ExchangeEvent, SymbolState};
use crate::strategy::Strategy;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Per-symbol 事件处理器
pub struct SymbolProcessor<S: Strategy> {
    symbol: Symbol,
    state: SymbolState,
    strategy: Arc<S>,
}

impl<S: Strategy + 'static> SymbolProcessor<S> {
    pub fn new(symbol: Symbol, strategy: Arc<S>) -> Self {
        Self {
            symbol: symbol.clone(),
            state: SymbolState::new(symbol),
            strategy,
        }
    }

    /// 启动处理循环
    pub fn run(
        mut self,
        mut event_rx: mpsc::Receiver<ExchangeEvent>,
        cancel_token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!(symbol = %self.symbol, "SymbolProcessor started");

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        tracing::info!(symbol = %self.symbol, "SymbolProcessor cancelled");
                        break;
                    }
                    event = event_rx.recv() => {
                        match event {
                            Some(evt) => {
                                self.handle_event(evt).await;
                            }
                            None => {
                                tracing::warn!(symbol = %self.symbol, "Event channel closed");
                                break;
                            }
                        }
                    }
                }
            }
        })
    }

    /// 处理单个事件
    async fn handle_event(&mut self, event: ExchangeEvent) {
        // 1. 更新状态
        self.state.apply(event.clone());

        // 2. 评估策略
        if let Some(signal) = self.strategy.evaluate(&self.state) {
            tracing::info!(
                symbol = %self.symbol,
                signal = ?signal,
                "Strategy signal generated"
            );

            // 3. 执行交易 (由 Coordinator 或 Strategy 实现)
            if let Err(e) = self.strategy.execute(signal).await {
                tracing::error!(
                    symbol = %self.symbol,
                    error = %e,
                    "Failed to execute signal"
                );
            }
        }
    }
}

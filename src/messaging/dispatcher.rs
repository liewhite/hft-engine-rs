use crate::domain::Exchange;
use crate::exchange::{PrivateSinks, PublicSinks};
use crate::messaging::event::ExchangeEvent;
use crate::messaging::event_bus::SymbolEventBus;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 事件分发器 - 从 Sinks 订阅事件并转发到 SymbolEventBus
pub struct EventDispatcher;

impl EventDispatcher {
    /// 从 PublicSinks 分发事件到 SymbolEventBus
    ///
    /// 为每个 symbol 的每种数据类型创建订阅任务
    pub fn dispatch_public(
        exchange: Exchange,
        sinks: &PublicSinks,
        event_bus: Arc<SymbolEventBus>,
        cancel_token: CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();

        for symbol in sinks.symbols() {
            // Funding rate updates per symbol
            if let Some(mut rx) = sinks.subscribe_funding_rate(&symbol) {
                let bus = event_bus.clone();
                let token = cancel_token.clone();
                let sym = symbol.clone();

                handles.push(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => break,
                            result = rx.recv() => {
                                match result {
                                    Ok(rate) => {
                                        let event = ExchangeEvent::FundingRateUpdate {
                                            symbol: sym.clone(),
                                            exchange,
                                            rate,
                                            timestamp: Instant::now(),
                                        };
                                        bus.publish(event);
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(
                                            exchange = %exchange,
                                            symbol = %sym,
                                            lagged = n,
                                            "Funding rate subscriber lagged"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }));
            }

            // BBO updates per symbol
            if let Some(mut rx) = sinks.subscribe_bbo(&symbol) {
                let bus = event_bus.clone();
                let token = cancel_token.clone();
                let sym = symbol.clone();

                handles.push(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => break,
                            result = rx.recv() => {
                                match result {
                                    Ok(bbo) => {
                                        let event = ExchangeEvent::BBOUpdate {
                                            symbol: sym.clone(),
                                            exchange,
                                            bbo,
                                            timestamp: Instant::now(),
                                        };
                                        bus.publish(event);
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(
                                            exchange = %exchange,
                                            symbol = %sym,
                                            lagged = n,
                                            "BBO subscriber lagged"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }));
            }
        }

        handles
    }

    /// 从 PrivateSinks 分发事件到 SymbolEventBus
    ///
    /// 为每个 symbol 的每种数据类型创建订阅任务
    pub fn dispatch_private(
        exchange: Exchange,
        sinks: &PrivateSinks,
        event_bus: Arc<SymbolEventBus>,
        cancel_token: CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();

        for symbol in sinks.symbols() {
            // Position updates per symbol
            if let Some(mut rx) = sinks.subscribe_position(&symbol) {
                let bus = event_bus.clone();
                let token = cancel_token.clone();
                let sym = symbol.clone();

                handles.push(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => break,
                            result = rx.recv() => {
                                match result {
                                    Ok(pos) => {
                                        let event = ExchangeEvent::PositionUpdate {
                                            symbol: sym.clone(),
                                            exchange,
                                            position: pos,
                                            timestamp: Instant::now(),
                                        };
                                        bus.publish(event);
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(
                                            exchange = %exchange,
                                            symbol = %sym,
                                            lagged = n,
                                            "Position subscriber lagged"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }));
            }

            // Order updates per symbol
            if let Some(mut rx) = sinks.subscribe_order_update(&symbol) {
                let bus = event_bus.clone();
                let token = cancel_token.clone();
                let sym = symbol.clone();

                handles.push(tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => break,
                            result = rx.recv() => {
                                match result {
                                    Ok(update) => {
                                        let event = ExchangeEvent::OrderStatusUpdate {
                                            symbol: sym.clone(),
                                            exchange,
                                            update,
                                            timestamp: Instant::now(),
                                        };
                                        bus.publish(event);
                                    }
                                    Err(broadcast::error::RecvError::Closed) => break,
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!(
                                            exchange = %exchange,
                                            symbol = %sym,
                                            lagged = n,
                                            "Order update subscriber lagged"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }));
            }
        }

        // Balance updates (全局事件，不按 symbol 分发)
        // Balance 需要单独处理，这里暂不分发到 per-symbol bus

        handles
    }

    /// 分发 Balance 事件 (不按 symbol 拆分)
    ///
    /// 返回订阅任务句柄，调用者可以选择如何处理 balance 事件
    pub fn dispatch_balance(
        exchange: Exchange,
        sinks: &PrivateSinks,
        cancel_token: CancellationToken,
        handler: impl Fn(crate::domain::Balance) + Send + 'static,
    ) -> JoinHandle<()> {
        let mut rx = sinks.subscribe_balance();
        let token = cancel_token;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    result = rx.recv() => {
                        match result {
                            Ok(balance) => {
                                handler(balance);
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(
                                    exchange = %exchange,
                                    lagged = n,
                                    "Balance subscriber lagged"
                                );
                            }
                        }
                    }
                }
            }
        })
    }
}

use crate::domain::Exchange;
use crate::exchange::{PrivateHubs, PublicHubs};
use crate::messaging::event::SymbolEvent;
use crate::messaging::event_bus::SymbolEventBus;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 事件分发器 - 从 Hub 订阅事件并转发到 SymbolEventBus
pub struct EventDispatcher;

impl EventDispatcher {
    /// 从 PublicHubs 分发事件到 SymbolEventBus
    pub fn dispatch_public(
        exchange: Exchange,
        hubs: &PublicHubs,
        event_bus: Arc<SymbolEventBus>,
        cancel_token: CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();

        // Funding rate updates
        {
            let mut rx = hubs.funding_rates.subscribe();
            let bus = event_bus.clone();
            let token = cancel_token.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(rate) => {
                                    let event = SymbolEvent::FundingRateUpdate {
                                        symbol: rate.symbol.clone(),
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
                                        lagged = n,
                                        "Funding rate subscriber lagged"
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }
            }));
        }

        // BBO updates
        {
            let mut rx = hubs.bbos.subscribe();
            let bus = event_bus.clone();
            let token = cancel_token.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(bbo) => {
                                    let event = SymbolEvent::BBOUpdate {
                                        symbol: bbo.symbol.clone(),
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
                                        lagged = n,
                                        "BBO subscriber lagged"
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }
            }));
        }

        handles
    }

    /// 从 PrivateHubs 分发事件到 SymbolEventBus
    pub fn dispatch_private(
        exchange: Exchange,
        hubs: &PrivateHubs,
        event_bus: Arc<SymbolEventBus>,
        cancel_token: CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();

        // Position updates
        {
            let mut rx = hubs.positions.subscribe();
            let bus = event_bus.clone();
            let token = cancel_token.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(pos) => {
                                    let event = SymbolEvent::PositionUpdate {
                                        symbol: pos.symbol.clone(),
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
                                        lagged = n,
                                        "Position subscriber lagged"
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }
            }));
        }

        // Order updates
        {
            let mut rx = hubs.order_updates.subscribe();
            let bus = event_bus.clone();
            let token = cancel_token.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(update) => {
                                    let event = SymbolEvent::OrderStatusUpdate {
                                        symbol: update.symbol.clone(),
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
                                        lagged = n,
                                        "Order update subscriber lagged"
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                }
            }));
        }

        // Balance updates (全局事件，不分发到 per-symbol bus)
        // Balance 需要单独处理，这里暂不分发

        handles
    }
}

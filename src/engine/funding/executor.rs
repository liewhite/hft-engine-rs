use crate::domain::{Exchange, Symbol};
use crate::exchange::{PrivateSinks, PublicDataType, PublicSinks};
use crate::messaging::{ExchangeEvent, StateManager};
use crate::strategy::{PublicStreams, Signal, Strategy};
use std::collections::HashSet;
use crate::domain::now_ms;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// 策略执行器 - 为单个策略订阅数据并执行
pub struct Executor {
    strategy: Box<dyn Strategy>,
    public_streams: PublicStreams,
}

impl Executor {
    pub fn new(strategy: Box<dyn Strategy>) -> Self {
        let public_streams = strategy.public_streams();

        Self {
            strategy,
            public_streams,
        }
    }

    /// 启动执行器
    pub fn run(
        mut self,
        public_sinks: &[(Exchange, PublicSinks)],
        private_sinks: &[(Exchange, PrivateSinks)],
        clock_rx: broadcast::Receiver<ExchangeEvent>,
        signal_tx: mpsc::Sender<Signal>,
        cancel_token: CancellationToken,
    ) {
        // 收集所有涉及的 symbols (用于 StateManager)
        let symbols: Vec<Symbol> = self
            .public_streams
            .values()
            .flat_map(|m| m.keys().cloned())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        // 创建 StateManager
        let order_timeout_ms = self.strategy.order_timeout_ms();
        let mut state_manager = StateManager::new(&symbols, signal_tx, order_timeout_ms);

        // 收集需要订阅的 receivers
        let mut receivers: Vec<tokio::sync::broadcast::Receiver<ExchangeEvent>> = Vec::new();

        // 根据 public_streams 订阅公共数据
        for (exchange, sinks) in public_sinks {
            if let Some(symbol_streams) = self.public_streams.get(exchange) {
                for (symbol, data_types) in symbol_streams {
                    if data_types.contains(&PublicDataType::BBO) {
                        if let Some(rx) = sinks.subscribe_bbo(symbol) {
                            receivers.push(wrap_bbo_receiver(*exchange, symbol.clone(), rx));
                        }
                    }
                    if data_types.contains(&PublicDataType::FundingRate) {
                        if let Some(rx) = sinks.subscribe_funding_rate(symbol) {
                            receivers.push(wrap_funding_receiver(*exchange, symbol.clone(), rx));
                        }
                    }
                }
            }
        }

        // 订阅私有数据 (Balance, Position, OrderUpdate)
        //
        // 私有数据的订阅范围由 public_streams 决定：
        // - 只为 public_streams 中存在的 exchange 订阅私有数据
        // - 只为 public_streams 中存在的 (exchange, symbol) 对订阅 Position/OrderUpdate
        // - Balance 按 exchange 订阅，不区分 symbol
        for (exchange, sinks) in private_sinks {
            if let Some(symbol_streams) = self.public_streams.get(exchange) {
                for symbol in symbol_streams.keys() {
                    if let Some(rx) = sinks.subscribe_position(symbol) {
                        receivers.push(wrap_position_receiver(*exchange, symbol.clone(), rx));
                    }
                    if let Some(rx) = sinks.subscribe_order_update(symbol) {
                        receivers.push(wrap_order_receiver(*exchange, symbol.clone(), rx));
                    }
                }

                receivers.push(wrap_balance_receiver(*exchange, sinks.subscribe_balance()));
            }
        }

        // 添加 clock receiver
        receivers.push(clock_rx);

        // 使用 mpsc 聚合所有 broadcast receivers
        let (event_tx, mut event_rx) = mpsc::channel::<ExchangeEvent>(256);

        for mut rx in receivers {
            let tx = event_tx.clone();
            let token = cancel_token.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(event) => {
                                    if tx.send(event).await.is_err() {
                                        break;
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            }
                        }
                    }
                }
            });
        }
        drop(event_tx);

        // 主循环：接收事件，更新状态，调用策略
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    event = event_rx.recv() => {
                        match event {
                            Some(evt) => {
                                // 1. 更新 StateManager 状态
                                state_manager.apply(&evt);
                                // 2. 调用策略处理事件
                                self.strategy.on_event(&evt, &mut state_manager);
                            }
                            None => break,
                        }
                    }
                }
            }
        });
    }
}

// 辅助函数：将具体类型的 receiver 转换为 ExchangeEvent receiver
fn wrap_bbo_receiver(
    exchange: Exchange,
    symbol: Symbol,
    mut rx: tokio::sync::broadcast::Receiver<crate::domain::BBO>,
) -> tokio::sync::broadcast::Receiver<ExchangeEvent> {
    let (tx, out_rx) = tokio::sync::broadcast::channel(256);
    tokio::spawn(async move {
        while let Ok(bbo) = rx.recv().await {
            let event = ExchangeEvent::BBOUpdate {
                symbol: symbol.clone(),
                exchange,
                bbo,
                timestamp: now_ms(),
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    out_rx
}

fn wrap_funding_receiver(
    exchange: Exchange,
    symbol: Symbol,
    mut rx: tokio::sync::broadcast::Receiver<crate::domain::FundingRate>,
) -> tokio::sync::broadcast::Receiver<ExchangeEvent> {
    let (tx, out_rx) = tokio::sync::broadcast::channel(256);
    tokio::spawn(async move {
        while let Ok(rate) = rx.recv().await {
            let event = ExchangeEvent::FundingRateUpdate {
                symbol: symbol.clone(),
                exchange,
                rate,
                timestamp: now_ms(),
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    out_rx
}

fn wrap_position_receiver(
    exchange: Exchange,
    symbol: Symbol,
    mut rx: tokio::sync::broadcast::Receiver<crate::domain::Position>,
) -> tokio::sync::broadcast::Receiver<ExchangeEvent> {
    let (tx, out_rx) = tokio::sync::broadcast::channel(256);
    tokio::spawn(async move {
        while let Ok(position) = rx.recv().await {
            let event = ExchangeEvent::PositionUpdate {
                symbol: symbol.clone(),
                exchange,
                position,
                timestamp: now_ms(),
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    out_rx
}

fn wrap_order_receiver(
    exchange: Exchange,
    symbol: Symbol,
    mut rx: tokio::sync::broadcast::Receiver<crate::domain::OrderUpdate>,
) -> tokio::sync::broadcast::Receiver<ExchangeEvent> {
    let (tx, out_rx) = tokio::sync::broadcast::channel(256);
    tokio::spawn(async move {
        while let Ok(update) = rx.recv().await {
            let event = ExchangeEvent::OrderStatusUpdate {
                symbol: symbol.clone(),
                exchange,
                update,
                timestamp: now_ms(),
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    out_rx
}

fn wrap_balance_receiver(
    exchange: Exchange,
    mut rx: tokio::sync::broadcast::Receiver<crate::domain::Balance>,
) -> tokio::sync::broadcast::Receiver<ExchangeEvent> {
    let (tx, out_rx) = tokio::sync::broadcast::channel(256);
    tokio::spawn(async move {
        while let Ok(balance) = rx.recv().await {
            let event = ExchangeEvent::BalanceUpdate {
                exchange,
                balance,
                timestamp: now_ms(),
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    out_rx
}

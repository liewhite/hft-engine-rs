use crate::domain::{Exchange, Symbol};
use crate::exchange::{PrivateSinks, PublicSinks};
use crate::messaging::{ExchangeEvent, StateManager};
use crate::strategy::{MarketDataType, Signal, Strategy};
use std::collections::HashSet;
use crate::domain::now_ms;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// 策略执行器 - 为单个策略订阅数据并执行
pub struct Executor {
    strategy: Box<dyn Strategy>,
    exchanges: HashSet<Exchange>,
    symbols: Vec<Symbol>,
    data_types: HashSet<MarketDataType>,
}

impl Executor {
    pub fn new(strategy: Box<dyn Strategy>) -> Self {
        let exchanges: HashSet<_> = strategy.exchanges().into_iter().collect();
        let symbols = strategy.symbols();
        let data_types: HashSet<_> = strategy.market_data_types().into_iter().collect();

        Self {
            strategy,
            exchanges,
            symbols,
            data_types,
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
        // 创建 StateManager
        let order_timeout_ms = self.strategy.order_timeout_ms();
        let mut state_manager = StateManager::new(&self.symbols, signal_tx, order_timeout_ms);

        // 收集需要订阅的 receivers
        let mut receivers: Vec<tokio::sync::broadcast::Receiver<ExchangeEvent>> = Vec::new();
        let symbols_set: HashSet<_> = self.symbols.iter().cloned().collect();

        for (exchange, sinks) in public_sinks {
            if !self.exchanges.contains(exchange) {
                continue;
            }

            for symbol in &symbols_set {
                if self.data_types.contains(&MarketDataType::BBO) {
                    if let Some(rx) = sinks.subscribe_bbo(symbol) {
                        receivers.push(wrap_bbo_receiver(*exchange, symbol.clone(), rx));
                    }
                }
                if self.data_types.contains(&MarketDataType::FundingRate) {
                    if let Some(rx) = sinks.subscribe_funding_rate(symbol) {
                        receivers.push(wrap_funding_receiver(*exchange, symbol.clone(), rx));
                    }
                }
            }
        }

        for (exchange, sinks) in private_sinks {
            if !self.exchanges.contains(exchange) {
                continue;
            }

            for symbol in &symbols_set {
                if self.data_types.contains(&MarketDataType::Position) {
                    if let Some(rx) = sinks.subscribe_position(symbol) {
                        receivers.push(wrap_position_receiver(*exchange, symbol.clone(), rx));
                    }
                }
                if self.data_types.contains(&MarketDataType::OrderUpdate) {
                    if let Some(rx) = sinks.subscribe_order_update(symbol) {
                        receivers.push(wrap_order_receiver(*exchange, symbol.clone(), rx));
                    }
                }
            }

            if self.data_types.contains(&MarketDataType::Balance) {
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

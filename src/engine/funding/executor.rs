use crate::domain::{Exchange, Order, OrderType, Side, Symbol, SymbolMeta};
use crate::exchange::{PrivateSinks, PublicDataType, PublicSinks};
use crate::messaging::{ExchangeEvent, StateManager};
use crate::strategy::{PublicStreams, Signal, Strategy};
use crate::domain::now_ms;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Symbol 元数据映射 (按交易所和 symbol 索引)
pub type SymbolMetas = Arc<HashMap<(Exchange, Symbol), SymbolMeta>>;

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
        symbol_metas: SymbolMetas,
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

        // 创建内部 signal channel 用于转换
        let (internal_signal_tx, mut internal_signal_rx) = mpsc::channel::<Signal>(256);

        // 创建 StateManager (使用内部 signal_tx)
        let order_timeout_ms = self.strategy.order_timeout_ms();
        let mut state_manager = StateManager::new(&symbols, internal_signal_tx, order_timeout_ms);

        // 启动 Signal 转换任务：coin_to_qty + round
        let metas_for_signal = symbol_metas.clone();
        let signal_token = cancel_token.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = signal_token.cancelled() => break,
                    signal = internal_signal_rx.recv() => {
                        match signal {
                            Some(Signal::PlaceOrder(order)) => {
                                let converted = convert_order_signal(&order, &metas_for_signal);
                                if signal_tx.send(Signal::PlaceOrder(converted)).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        // 收集需要订阅的 receivers
        let mut receivers: Vec<tokio::sync::broadcast::Receiver<ExchangeEvent>> = Vec::new();

        // 根据 public_streams 订阅公共数据
        for (exchange, sinks) in public_sinks {
            if let Some(symbol_streams) = self.public_streams.get(exchange) {
                for (symbol, data_types) in symbol_streams {
                    let ex = *exchange;
                    let sym = symbol.clone();

                    if data_types.contains(&PublicDataType::BBO) {
                        if let Some(rx) = sinks.subscribe_bbo(symbol) {
                            let sym = sym.clone();
                            receivers.push(wrap_receiver(rx, move |bbo| ExchangeEvent::BBOUpdate {
                                symbol: sym.clone(),
                                exchange: ex,
                                bbo,
                                timestamp: now_ms(),
                            }));
                        }
                    }
                    if data_types.contains(&PublicDataType::FundingRate) {
                        if let Some(rx) = sinks.subscribe_funding_rate(symbol) {
                            let sym = sym.clone();
                            receivers.push(wrap_receiver(rx, move |rate| {
                                ExchangeEvent::FundingRateUpdate {
                                    symbol: sym.clone(),
                                    exchange: ex,
                                    rate,
                                    timestamp: now_ms(),
                                }
                            }));
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
                let ex = *exchange;

                for symbol in symbol_streams.keys() {
                    let sym = symbol.clone();

                    if let Some(rx) = sinks.subscribe_position(symbol) {
                        let sym = sym.clone();
                        let meta = symbol_metas.get(&(ex, sym.clone())).cloned();
                        receivers.push(wrap_receiver(rx, move |mut position| {
                            // 将 position size 从张数转换为币数量
                            if let Some(ref m) = meta {
                                position.size = m.qty_to_coin(position.size);
                            }
                            ExchangeEvent::PositionUpdate {
                                symbol: sym.clone(),
                                exchange: ex,
                                position,
                                timestamp: now_ms(),
                            }
                        }));
                    }
                    if let Some(rx) = sinks.subscribe_order_update(symbol) {
                        let sym = sym.clone();
                        receivers.push(wrap_receiver(rx, move |update| {
                            ExchangeEvent::OrderStatusUpdate {
                                symbol: sym.clone(),
                                exchange: ex,
                                update,
                                timestamp: now_ms(),
                            }
                        }));
                    }
                }

                receivers.push(wrap_receiver(sinks.subscribe_balance(), move |balance| {
                    ExchangeEvent::BalanceUpdate {
                        exchange: ex,
                        balance,
                        timestamp: now_ms(),
                    }
                }));
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

/// 通用 receiver 包装器：将具体类型的 receiver 转换为 ExchangeEvent receiver
///
/// 使用闭包处理转换逻辑，消除重复的 channel 创建和 spawn 代码
fn wrap_receiver<T, F>(
    mut rx: broadcast::Receiver<T>,
    transform: F,
) -> broadcast::Receiver<ExchangeEvent>
where
    T: Clone + Send + 'static,
    F: Fn(T) -> ExchangeEvent + Send + 'static,
{
    let (tx, out_rx) = broadcast::channel(256);
    tokio::spawn(async move {
        while let Ok(item) = rx.recv().await {
            if tx.send(transform(item)).is_err() {
                break;
            }
        }
    });
    out_rx
}

/// 转换订单信号：coin_to_qty + round
///
/// - quantity: 从币数量转换为张数并向下取整
/// - price (Limit): 买单向上取整，卖单向下取整
fn convert_order_signal(
    order: &Order,
    symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
) -> Order {
    let key = (order.exchange, order.symbol.clone());
    let meta = match symbol_metas.get(&key) {
        Some(m) => m,
        None => {
            tracing::warn!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                "SymbolMeta not found, order not converted"
            );
            return order.clone();
        }
    };

    // 转换并取整 quantity
    let qty_in_contracts = meta.coin_to_qty(order.quantity);
    let rounded_qty = meta.round_size_down(qty_in_contracts);

    // 转换 price (仅 Limit 订单)
    let converted_order_type = match &order.order_type {
        OrderType::Market => OrderType::Market,
        OrderType::Limit { price, tif } => {
            // 买单向上取整确保能成交，卖单向下取整确保能成交
            let rounded_price = match order.side {
                Side::Long => meta.round_price_up(*price),
                Side::Short => meta.round_price_down(*price),
            };
            OrderType::Limit {
                price: rounded_price,
                tif: *tif,
            }
        }
    };

    Order {
        id: order.id.clone(),
        exchange: order.exchange,
        symbol: order.symbol.clone(),
        side: order.side,
        order_type: converted_order_type,
        quantity: rounded_qty,
        reduce_only: order.reduce_only,
        client_order_id: order.client_order_id.clone(),
    }
}

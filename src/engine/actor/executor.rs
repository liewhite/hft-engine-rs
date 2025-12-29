//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 ExchangeEvent，调用 Strategy.on_event()

use crate::domain::{Exchange, Order, Symbol, SymbolMeta};
use crate::exchange::SignalSink;
use crate::messaging::{ExchangeEvent, StateManager};
use crate::strategy::{Signal, Strategy};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// ExecutorActor 初始化参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// Symbol 元数据 (用于 qty 转换)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// Signal 接收器 (发送到 SignalProcessorActor)
    pub signal_sink: Arc<dyn SignalSink>,
}

/// ExecutorActor - 执行策略的 Actor
pub struct ExecutorActor {
    /// 策略实例
    strategy: Box<dyn Strategy>,
    /// 状态管理器
    state_manager: StateManager,
    /// Signal 接收器
    signal_sink: Arc<dyn SignalSink>,
    /// Symbol 元数据 (用于 qty 转换)
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 内部 Signal 接收器 (用于收集 StateManager 产生的 Signal)
    signal_rx: mpsc::Receiver<Signal>,
}

impl ExecutorActor {
    /// 创建 ExecutorActor
    pub fn new(args: ExecutorArgs) -> Self {
        // 从策略获取订阅的 symbols
        let public_streams = args.strategy.public_streams();
        let symbols: Vec<Symbol> = public_streams
            .values()
            .flat_map(|m| m.keys().cloned())
            .collect();

        // 创建内部 signal channel (StateManager 将 Signal 发送到这里)
        let (internal_tx, signal_rx) = mpsc::channel::<Signal>(256);

        // 创建状态管理器
        let order_timeout_ms = args.strategy.order_timeout_ms();
        let state_manager = StateManager::new(&symbols, internal_tx, order_timeout_ms);

        Self {
            strategy: args.strategy,
            state_manager,
            signal_sink: args.signal_sink,
            symbol_metas: args.symbol_metas,
            signal_rx,
        }
    }

    /// 处理 ExchangeEvent，调用策略
    async fn handle_event(&mut self, event: ExchangeEvent) {
        self.state_manager.apply(&event);
        self.strategy.on_event(&event, &mut self.state_manager);

        // 收集并处理产生的 Signal
        self.process_pending_signals().await;
    }

    /// 处理内部 channel 中待处理的 Signal
    async fn process_pending_signals(&mut self) {
        // 非阻塞地从 channel 中读取所有待处理的 signal
        while let Ok(signal) = self.signal_rx.try_recv() {
            // 转换 Signal (qty 转换)
            let converted = match signal {
                Signal::PlaceOrder(order) => {
                    Signal::PlaceOrder(convert_order(&order, &self.symbol_metas))
                }
            };

            // 发送到 SignalSink
            self.signal_sink.send_signal(converted).await;
        }
    }
}

impl Actor for ExecutorActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ExecutorActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::info!("ExecutorActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("ExecutorActor stopped");
        Ok(())
    }
}

// === Messages ===

/// ExchangeEvent 消息 - 从 ProcessorActor 接收 (包含所有事件类型，含 Clock)
impl Message<ExchangeEvent> for ExecutorActor {
    type Reply = ();

    async fn handle(&mut self, msg: ExchangeEvent, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_event(msg).await;
    }
}

// === Helper Functions ===

/// 转换订单：coin_to_qty + round
fn convert_order(order: &Order, metas: &HashMap<(Exchange, Symbol), SymbolMeta>) -> Order {
    use crate::domain::{OrderType, Side};

    let key = (order.exchange, order.symbol.clone());
    let meta = match metas.get(&key) {
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

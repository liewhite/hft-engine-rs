//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 MarketData，转换为 ExchangeEvent，调用 Strategy.on_event()

use crate::domain::{now_ms, Exchange, Order, Symbol, SymbolMeta};
use crate::exchange::MarketData;
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
    /// Signal 输出 channel
    pub signal_tx: mpsc::Sender<Signal>,
}

/// ExecutorActor - 执行策略的 Actor
pub struct ExecutorActor {
    /// 策略实例
    strategy: Box<dyn Strategy>,
    /// 状态管理器
    state_manager: StateManager,
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

        // 创建内部 signal channel (用于 qty 转换)
        let (internal_tx, internal_rx) = mpsc::channel::<Signal>(256);

        // 启动 qty 转换任务
        let metas = args.symbol_metas.clone();
        let external_tx = args.signal_tx.clone();
        tokio::spawn(async move {
            signal_converter(internal_rx, external_tx, metas).await;
        });

        // 创建状态管理器
        let order_timeout_ms = args.strategy.order_timeout_ms();
        let state_manager = StateManager::new(&symbols, internal_tx, order_timeout_ms);

        Self {
            strategy: args.strategy,
            state_manager,
        }
    }

    /// 处理 MarketData，转换为 ExchangeEvent 并调用策略
    fn handle_market_data(&mut self, data: MarketData) {
        let event = market_data_to_event(data);
        self.state_manager.apply(&event);
        self.strategy.on_event(&event, &mut self.state_manager);
    }

    /// 处理 Clock 事件
    fn handle_clock(&mut self, timestamp: u64) {
        let event = ExchangeEvent::Clock { timestamp };
        self.state_manager.apply(&event);
        self.strategy.on_event(&event, &mut self.state_manager);
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

/// MarketData 消息 - 从 SubscriberActor 接收
impl Message<MarketData> for ExecutorActor {
    type Reply = ();

    async fn handle(&mut self, msg: MarketData, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_market_data(msg);
    }
}

/// Clock 消息 - 定时时钟
#[derive(Clone)]
pub struct ClockTick {
    pub timestamp: u64,
}

impl Message<ClockTick> for ExecutorActor {
    type Reply = ();

    async fn handle(&mut self, msg: ClockTick, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_clock(msg.timestamp);
    }
}

// === Helper Functions ===

/// 将 MarketData 转换为 ExchangeEvent
fn market_data_to_event(data: MarketData) -> ExchangeEvent {
    let timestamp = now_ms();

    match data {
        MarketData::FundingRate {
            exchange,
            symbol,
            rate,
        } => ExchangeEvent::FundingRateUpdate {
            symbol,
            exchange,
            rate,
            timestamp,
        },
        MarketData::BBO {
            exchange,
            symbol,
            bbo,
        } => ExchangeEvent::BBOUpdate {
            symbol,
            exchange,
            bbo,
            timestamp,
        },
        MarketData::Position {
            exchange,
            symbol,
            position,
        } => ExchangeEvent::PositionUpdate {
            symbol,
            exchange,
            position,
            timestamp,
        },
        MarketData::Balance { exchange, balance } => ExchangeEvent::BalanceUpdate {
            exchange,
            balance,
            timestamp,
        },
        MarketData::OrderUpdate {
            exchange,
            symbol,
            update,
        } => ExchangeEvent::OrderStatusUpdate {
            symbol,
            exchange,
            update,
            timestamp,
        },
        MarketData::Equity { exchange, value } => ExchangeEvent::EquityUpdate {
            exchange,
            equity: value,
            timestamp,
        },
    }
}

/// Signal 转换任务：coin_to_qty + round
async fn signal_converter(
    mut rx: mpsc::Receiver<Signal>,
    tx: mpsc::Sender<Signal>,
    metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
) {
    while let Some(signal) = rx.recv().await {
        match signal {
            Signal::PlaceOrder(order) => {
                let converted = convert_order(&order, &metas);
                if tx.send(Signal::PlaceOrder(converted)).await.is_err() {
                    break;
                }
            }
        }
    }
}

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

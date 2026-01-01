//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 ExchangeEvent，调用 Strategy.on_event()

use crate::domain::{Exchange, Order, OrderType, Side, Symbol, SymbolMeta};
use crate::engine::SignalProcessorActor;
use crate::messaging::{IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// ExecutorActor 初始化参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// Symbol 元数据 (用于 qty 转换)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// SignalProcessorActor 引用 (用于下单)
    pub signal_processor: ActorRef<SignalProcessorActor>,
}

/// ExecutorActor - 执行策略的 Actor
pub struct ExecutorActor {
    /// 策略实例
    strategy: Box<dyn Strategy>,
    /// 状态管理器
    state_manager: StateManager,
    /// Symbol 元数据 (用于订单转换)
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// SignalProcessorActor 引用 (用于下单)
    signal_processor: ActorRef<SignalProcessorActor>,
}

impl ExecutorActor {
    /// 创建 ExecutorActor
    pub fn new(args: ExecutorArgs) -> Self {
        // 从策略获取订阅的 symbols (去重)
        let public_streams = args.strategy.public_streams();
        let symbols: Vec<Symbol> = public_streams
            .values()
            .flat_map(|kinds| kinds.iter().map(|k| k.symbol().clone()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // 创建状态管理器
        let order_timeout_ms = args.strategy.order_timeout_ms();
        let state_manager = StateManager::new(&symbols, order_timeout_ms);

        Self {
            strategy: args.strategy,
            state_manager,
            symbol_metas: args.symbol_metas,
            signal_processor: args.signal_processor,
        }
    }

    /// 转换订单：生成 client_order_id，coin_to_qty + round price/size
    fn convert_order(&self, mut order: Order) -> Order {
        // 生成 client_order_id (去掉 `-`，OKX 只允许字母数字)
        order.client_order_id = Uuid::new_v4().simple().to_string();

        let key = (order.exchange, order.symbol.clone());
        let meta = match self.symbol_metas.get(&key) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    "SymbolMeta not found, order not converted"
                );
                return order;
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
            id: order.id,
            exchange: order.exchange,
            symbol: order.symbol,
            side: order.side,
            order_type: converted_order_type,
            quantity: rounded_qty,
            reduce_only: order.reduce_only,
            client_order_id: order.client_order_id,
        }
    }

    /// 处理 ExchangeEvent，调用策略并处理返回的信号
    async fn handle_event(&mut self, event: IncomeEvent) {
        // 先更新状态
        self.state_manager.apply(&event);

        // 调用策略，获取信号
        let signals = self.strategy.on_event(&event, &self.state_manager);

        // 处理每个信号
        for signal in signals {
            match signal {
                OutcomeEvent::PlaceOrder(order) => {
                    // 转换订单
                    let converted_order = self.convert_order(order);
                    // 添加到 pending_orders
                    self.state_manager.add_pending_order(
                        &converted_order.symbol,
                        converted_order.client_order_id.clone(),
                        converted_order.exchange,
                    );
                    // 发送到 SignalProcessor
                    self.signal_processor
                        .tell(OutcomeEvent::PlaceOrder(converted_order))
                        .await
                        .expect("Failed to send order to SignalProcessorActor");
                }
            }
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
impl Message<IncomeEvent> for ExecutorActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_event(msg).await;
    }
}

//! SignalProcessorActor - 处理策略信号并执行下单
//!
//! 接收 Signal 消息，调用交易所 REST API 执行订单

use crate::domain::{now_ms, Exchange, OrderStatus, OrderUpdate};
use crate::exchange::{EventSink, ExchangeClient};
use crate::messaging::{ExchangeEvent, ExchangeEventData};
use crate::strategy::Signal;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// SignalProcessorActor 初始化参数
pub struct SignalProcessorArgs {
    /// 交易所客户端映射
    pub clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 事件接收器（用于发送下单失败事件）
    pub event_sink: Arc<dyn EventSink>,
}

/// SignalProcessorActor - 处理交易信号
pub struct SignalProcessorActor {
    /// 交易所客户端
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
}

impl SignalProcessorActor {
    /// 创建 SignalProcessorActor
    pub fn new(args: SignalProcessorArgs) -> Self {
        Self {
            clients: args.clients,
            event_sink: args.event_sink,
        }
    }
}

impl Actor for SignalProcessorActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "SignalProcessorActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::info!("SignalProcessorActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("SignalProcessorActor stopped");
        Ok(())
    }
}

// === Message Handlers ===

impl Message<Signal> for SignalProcessorActor {
    type Reply = ();

    async fn handle(&mut self, msg: Signal, _ctx: Context<'_, Self, Self::Reply>) {
        match msg {
            Signal::PlaceOrder(order) => {
                let client = match self.clients.get(&order.exchange) {
                    Some(e) => e.clone(),
                    None => {
                        let reason = format!("No client found for exchange {}", order.exchange);
                        tracing::error!(
                            exchange = %order.exchange,
                            "{}", reason
                        );
                        // 发送错误事件
                        self.send_order_error(&order, reason).await;
                        return;
                    }
                };

                tracing::info!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    side = %order.side,
                    order_type = ?order.order_type,
                    quantity = order.quantity,
                    client_order_id = ?order.client_order_id,
                    "Placing order"
                );

                // 直接 await 下单请求
                match client.place_order(order.clone()).await {
                    Ok(order_id) => {
                        tracing::info!(
                            exchange = %order.exchange,
                            symbol = %order.symbol,
                            order_id = %order_id,
                            client_order_id = ?order.client_order_id,
                            "Order placed successfully"
                        );
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        tracing::error!(
                            exchange = %order.exchange,
                            symbol = %order.symbol,
                            client_order_id = ?order.client_order_id,
                            error = %reason,
                            "Failed to place order"
                        );
                        // 发送错误事件
                        self.send_order_error(&order, reason).await;
                    }
                }
            }
        }
    }
}

impl SignalProcessorActor {
    /// 发送订单错误事件
    async fn send_order_error(&self, order: &crate::domain::Order, reason: String) {
        let local_ts = now_ms();
        let update = OrderUpdate {
            order_id: String::new(),
            client_order_id: order.client_order_id.clone(),
            exchange: order.exchange,
            symbol: order.symbol.clone(),
            status: OrderStatus::Error { reason },
            filled_quantity: 0.0,
            avg_price: None,
            timestamp: local_ts,
        };

        self.event_sink
            .send_event(ExchangeEvent {
                exchange_ts: local_ts, // 本地错误，没有交易所时间戳
                local_ts,
                data: ExchangeEventData::OrderUpdate(update),
            })
            .await;
    }
}

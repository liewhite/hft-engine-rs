//! SignalProcessorActor - 处理策略信号并执行下单
//!
//! 接收 Signal 消息，调用交易所 REST API 执行订单

use crate::domain::Exchange;
use crate::exchange::ExchangeClient;
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
    pub executors: HashMap<Exchange, Arc<dyn ExchangeClient>>,
}

/// SignalProcessorActor - 处理交易信号
pub struct SignalProcessorActor {
    /// 交易所客户端
    executors: HashMap<Exchange, Arc<dyn ExchangeClient>>,
}

impl SignalProcessorActor {
    /// 创建 SignalProcessorActor
    pub fn new(args: SignalProcessorArgs) -> Self {
        Self {
            executors: args.executors,
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
                let client = match self.executors.get(&order.exchange) {
                    Some(e) => e.clone(),
                    None => {
                        tracing::error!(
                            exchange = %order.exchange,
                            "No client found for exchange"
                        );
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

                // 异步执行下单 (spawn 避免阻塞 actor)
                let order_clone = order.clone();
                tokio::spawn(async move {
                    match client.place_order(order_clone.clone()).await {
                        Ok(order_id) => {
                            tracing::info!(
                                exchange = %order_clone.exchange,
                                symbol = %order_clone.symbol,
                                order_id = %order_id,
                                client_order_id = ?order_clone.client_order_id,
                                "Order placed successfully"
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                exchange = %order_clone.exchange,
                                symbol = %order_clone.symbol,
                                client_order_id = ?order_clone.client_order_id,
                                error = %e,
                                "Failed to place order"
                            );
                        }
                    }
                });
            }
        }
    }
}

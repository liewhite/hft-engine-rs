//! SignalProcessorActor - 处理策略信号并执行下单
//!
//! 订阅 OutcomePubSub 接收策略信号，调用交易所 REST API 执行订单

use crate::domain::{now_ms, Exchange, OrderStatus, OrderUpdate};
use crate::exchange::ExchangeClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::OutcomeEvent;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::sync::Arc;

use super::IncomePubSub;

/// SignalProcessorActor 初始化参数
pub struct SignalProcessorArgs {
    /// 交易所客户端映射
    pub clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub（用于发布下单失败事件）
    pub income_pubsub: ActorRef<IncomePubSub>,
}

/// SignalProcessorActor - 处理交易信号
pub struct OutcomeProcessorActor {
    /// 交易所客户端
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub
    income_pubsub: ActorRef<IncomePubSub>,
}

impl Actor for OutcomeProcessorActor {
    type Args = SignalProcessorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::info!("SignalProcessorActor started");
        Ok(Self {
            clients: args.clients,
            income_pubsub: args.income_pubsub,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("SignalProcessorActor stopped");
        Ok(())
    }
}

// === Message Handlers ===

impl Message<OutcomeEvent> for OutcomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: OutcomeEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match msg {
            OutcomeEvent::PlaceOrder { order, comment } => {
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
                    comment = %comment,
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
                        // reduce_only 订单因仓位已平而被拒绝是正常的竞态情况
                        if reason.contains("Reduce only") || reason.contains("reduce only") {
                            tracing::info!(
                                exchange = %order.exchange,
                                symbol = %order.symbol,
                                client_order_id = ?order.client_order_id,
                                "Reduce-only order rejected: position already closed"
                            );
                        } else {
                            tracing::error!(
                                exchange = %order.exchange,
                                symbol = %order.symbol,
                                client_order_id = ?order.client_order_id,
                                error = %reason,
                                "Failed to place order"
                            );
                        }
                        // 发送错误事件
                        self.send_order_error(&order, reason).await;
                    }
                }
            }
        }
    }
}

impl OutcomeProcessorActor {
    /// 发送订单错误事件
    async fn send_order_error(&self, order: &crate::domain::Order, reason: String) {
        let local_ts = now_ms();
        let update = OrderUpdate {
            order_id: String::new(),
            client_order_id: Some(order.client_order_id.clone()),
            exchange: order.exchange,
            symbol: order.symbol.clone(),
            side: order.side,
            status: OrderStatus::Error { reason },
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: local_ts,
        };

        let _ = self
            .income_pubsub
            .tell(Publish(IncomeEvent {
                exchange_ts: local_ts, // 本地错误，没有交易所时间戳
                local_ts,
                data: ExchangeEventData::OrderUpdate(update),
            }))
            .send()
            .await;
    }
}

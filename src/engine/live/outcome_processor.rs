//! OutcomeProcessorActor - 处理策略信号并执行下单
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

/// OutcomeProcessorActor 初始化参数
pub struct OutcomeProcessorArgs {
    /// 交易所客户端映射
    pub clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub（用于发布下单失败事件）
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// dry-run 模式：只打日志不下单
    pub dry_run: bool,
}

/// OutcomeProcessorActor - 处理策略信号并执行下单
pub struct OutcomeProcessorActor {
    /// 交易所客户端
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub
    income_pubsub: ActorRef<IncomePubSub>,
    /// dry-run 模式
    dry_run: bool,
}

impl Actor for OutcomeProcessorActor {
    type Args = OutcomeProcessorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        if args.dry_run {
            tracing::warn!("OutcomeProcessorActor started in DRY-RUN mode (orders will NOT be placed)");
        } else {
            tracing::info!("OutcomeProcessorActor started");
        }
        Ok(Self {
            clients: args.clients,
            income_pubsub: args.income_pubsub,
            dry_run: args.dry_run,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("OutcomeProcessorActor stopped");
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
            OutcomeEvent::CancelOrder { exchange, symbol, order_id } => {
                let client = match self.clients.get(&exchange) {
                    Some(e) => e.clone(),
                    None => {
                        tracing::error!(%exchange, "No client found for cancel_order");
                        return;
                    }
                };

                if self.dry_run {
                    tracing::warn!(%exchange, %symbol, %order_id, "[DRY-RUN] CancelOrder NOT sent");
                    return;
                }

                tracing::info!(%exchange, %symbol, %order_id, "Cancelling order");
                tokio::spawn(async move {
                    match client.cancel_order(&symbol, &order_id).await {
                        Ok(()) => {
                            tracing::info!(%exchange, %symbol, %order_id, "Order cancelled successfully");
                        }
                        Err(e) => {
                            tracing::error!(%exchange, %symbol, %order_id, error = %e, "Failed to cancel order");
                        }
                    }
                });
            }
            OutcomeEvent::PlaceOrders { orders, comment } => {
                // 关联订单独立并行下单：IOC 订单本身接受部分成交，
                // 敞口由策略层 rebalance 机制兜底修正。
                for order in orders {
                    let client = match self.clients.get(&order.exchange) {
                        Some(e) => e.clone(),
                        None => {
                            let reason =
                                format!("No client found for exchange {}", order.exchange);
                            tracing::error!(
                                exchange = %order.exchange,
                                "{}", reason
                            );
                            self.send_order_error(&order, reason).await;
                            continue;
                        }
                    };

                    if self.dry_run {
                        tracing::warn!(
                            exchange = %order.exchange,
                            symbol = %order.symbol,
                            side = %order.side,
                            order_type = ?order.order_type,
                            quantity = order.quantity,
                            client_order_id = ?order.client_order_id,
                            signal = %comment,
                            "[DRY-RUN] Order NOT placed"
                        );
                        continue;
                    }

                    tracing::info!(
                        exchange = %order.exchange,
                        symbol = %order.symbol,
                        side = %order.side,
                        order_type = ?order.order_type,
                        quantity = order.quantity,
                        client_order_id = ?order.client_order_id,
                        signal = %comment,
                        "Placing order"
                    );

                    let income_pubsub = self.income_pubsub.clone();
                    tokio::spawn(async move {
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
                                if reason.contains("Reduce only")
                                    || reason.contains("reduce only")
                                {
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
                                Self::send_order_error_static(&income_pubsub, &order, reason)
                                    .await;
                            }
                        }
                    });
                }
            }
        }
    }
}

impl OutcomeProcessorActor {
    /// 发送订单错误事件
    async fn send_order_error(&self, order: &crate::domain::Order, reason: String) {
        Self::send_order_error_static(&self.income_pubsub, order, reason).await;
    }

    /// 发送订单错误事件（静态版本，用于 tokio::spawn）
    async fn send_order_error_static(
        income_pubsub: &ActorRef<IncomePubSub>,
        order: &crate::domain::Order,
        reason: String,
    ) {
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

        if let Err(e) = income_pubsub
            .tell(Publish(IncomeEvent {
                exchange_ts: local_ts, // 本地错误，没有交易所时间戳
                local_ts,
                data: ExchangeEventData::OrderUpdate(update),
            }))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish to IncomePubSub");
        }
    }
}

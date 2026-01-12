//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 IncomeEvent，调用 Strategy.on_event()，发布 OutcomeEvent 到 OutcomePubSub

use crate::domain::{Exchange, Order, OrderType, Symbol, SymbolMeta};
use crate::messaging::{IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::sync::Arc;

use super::OutcomePubSub;

/// ExecutorActor 初始化参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// Symbol 元数据 (用于 qty 转换)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// Outcome PubSub 引用 (用于发布信号)
    pub outcome_pubsub: ActorRef<OutcomePubSub>,
}

/// ExecutorActor - 执行策略的 Actor
pub struct ExecutorActor {
    /// 策略实例
    strategy: Box<dyn Strategy>,
    /// 状态管理器
    state_manager: StateManager,
    /// Symbol 元数据 (用于订单转换)
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// Outcome PubSub 引用 (用于发布信号)
    outcome_pubsub: ActorRef<OutcomePubSub>,
}

impl ExecutorActor {
    /// 转换订单：生成 client_order_id，coin_to_qty + round price/size
    fn convert_order(&self, mut order: Order) -> Order {
        // 生成交易所特定的 client_order_id
        order.client_order_id = order.exchange.new_cli_order_id();

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
                let rounded_price = meta.round_price(*price);
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

    /// 处理 IncomeEvent，调用策略并处理返回的信号
    async fn handle_event(&mut self, event: IncomeEvent) {
        // 先更新状态
        self.state_manager.apply(&event);

        // 调用策略，获取信号
        let signals = self.strategy.on_event(&event, &self.state_manager);

        // 处理每个信号
        for signal in signals {
            match signal {
                OutcomeEvent::PlaceOrder { order, comment } => {
                    // 转换订单
                    let converted_order = self.convert_order(order);
                    // 添加到 pending_orders（用于追踪订单状态）
                    self.state_manager.add_pending_order(
                        &converted_order.symbol,
                        converted_order.client_order_id.clone(),
                        converted_order.exchange,
                    );
                    // 发布到 OutcomePubSub
                    let _ = self
                        .outcome_pubsub
                        .tell(Publish(OutcomeEvent::PlaceOrder {
                            order: converted_order,
                            comment,
                        }))
                        .send()
                        .await;
                }
            }
        }
    }
}

impl Actor for ExecutorActor {
    type Args = ExecutorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
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

        tracing::info!("ExecutorActor started");
        Ok(Self {
            strategy: args.strategy,
            state_manager,
            symbol_metas: args.symbol_metas,
            outcome_pubsub: args.outcome_pubsub,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("ExecutorActor stopped");
        Ok(())
    }
}

// === Messages ===

/// IncomeEvent 消息 - 从 ProcessorActor 接收 (包含所有事件类型，含 Clock)
impl Message<IncomeEvent> for ExecutorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: IncomeEvent,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_event(msg).await;
    }
}

//! OutcomeProcessorActor - 处理策略信号并执行下单
//!
//! 订阅 OutcomePubSub 接收策略信号，调用交易所 REST API 执行订单。
//!
//! **系统的出向单位边界**：策略与 `StrategyRunner` 产出的订单一律币本位，在此转为
//! [`ExchangeOrder`]（折算 + 按交易所精度取整）后才发给交易所。回测不经过本 actor，
//! 因此 `SimState` 拿到的同样是币本位订单 —— 两条路径在策略侧口径一致。

use crate::domain::{
    now_ms, Exchange, ExchangeError, OrderStatus, OrderUpdate, RejectReason, Side, Symbol,
    SymbolMeta,
};
use crate::exchange::{ExchangeClient, ExchangeOrder};
use crate::domain::AccountId;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::OutcomeEvent;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::sync::Arc;

use super::{AccountOutcome, IncomePubSub};

/// OutcomeProcessorActor 初始化参数
pub struct OutcomeProcessorArgs {
    /// 交易所客户端映射
    pub clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub（用于发布下单失败事件）
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据：用于把币本位订单折算为交易所下单单位并取整
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// dry-run 模式：只打日志不下单
    pub dry_run: bool,
}

/// OutcomeProcessorActor - 处理策略信号并执行下单
pub struct OutcomeProcessorActor {
    /// 交易所客户端
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据（出向单位折算）
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
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
            symbol_metas: args.symbol_metas,
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

impl Message<AccountOutcome> for OutcomeProcessorActor {
    type Reply = ();

    async fn handle(
        &mut self,
        tagged: AccountOutcome,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 只负责实盘账户；模拟账户的订单由 PaperCounterActor 处理。
        // 二者共用一条 OutcomePubSub，各按账户取自己的那份（见 AccountOutcome）。
        if tagged.account != AccountId::Live {
            return;
        }
        let msg = tagged.event;
        // 下单/撤单一律 `tokio::spawn` 异步执行，**有意为之**：REST 往返可能上百 ms，
        // 若在 handler 内 await 会阻塞本 actor 的邮箱，进而拖垮整条 income/outcome 事件流
        // （行情、成交回报全部积压）。
        //
        // 已知取舍（当前不处理，靠上层机制兜底）：
        // - 同一 symbol 的多笔下单/撤单可能乱序到达交易所——策略层的 rebalance /
        //   reduce-only 会在后续事件中纠正净敞口，不依赖单条请求的到达序。
        // - 优雅停机时 `on_stop` 不等待 in-flight 请求，理论上存在"引擎已停但请求仍在途"
        //   的窗口；接受此风险以换取事件流不阻塞。
        // - spawned task 内的失败已通过 `send_order_error*` 反馈为 OrderUpdate(Error/Rejected)，
        //   不会静默丢失。
        match msg {
            OutcomeEvent::CancelOrder { exchange, symbol, order_id, client_order_id } => {
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
                let income_pubsub = self.income_pubsub.clone();
                tokio::spawn(async move {
                    match client.cancel_order(&symbol, &order_id).await {
                        Ok(()) => {
                            tracing::info!(%exchange, %symbol, %order_id, "Order cancelled successfully");
                            // 撤单成功，合成 Cancelled 回报让 SymbolState 移除 pending。
                            // 必须携带真实的 client_order_id —— pending 以它为键，填错
                            // （此前填的是交易所 order_id）会让撤掉的单永远留在本地。
                            Self::publish_cancelled(
                                &income_pubsub,
                                exchange,
                                symbol,
                                order_id,
                                client_order_id,
                            )
                            .await;
                        }
                        Err(e) => {
                            // 撤单失败最常见的原因是订单已终态（成交 / 已撤 / 不存在）。
                            // 若它的终态回报恰好丢失，本地 pending 会让 has_pending_orders
                            // 恒真、该 symbol 永久冻结（Pending 态没有超时清理）。用挂单
                            // 列表复查一次把这条路堵上：
                            // - 单仍挂着：撤单真失败了，pending 保留是正确的，报 error；
                            // - 单已不在：已终态。合成 Cancelled 清本地 pending —— 若真实
                            //   终态是成交，其 Fill 独立更新仓位、不经 pending，合成回报
                            //   不会盖掉任何信息。
                            match client.fetch_pending_orders(&symbol).await {
                                Ok(open_orders) => match cancel_recheck_verdict(
                                    &open_orders,
                                    &order_id,
                                    &client_order_id,
                                ) {
                                    CancelRecheckVerdict::StillOpen => {
                                        tracing::error!(
                                            %exchange, %symbol, %order_id, error = %e,
                                            "撤单失败且订单仍挂在交易所，保留本地 pending，待策略重试"
                                        );
                                    }
                                    CancelRecheckVerdict::Unverifiable => {
                                        tracing::error!(
                                            %exchange, %symbol, %order_id, error = %e,
                                            "撤单失败且挂单列表里存在无法比对身份的挂单，\
                                             保留本地 pending（误清活单比误留死单危险）"
                                        );
                                    }
                                    CancelRecheckVerdict::Gone => {
                                        tracing::warn!(
                                            %exchange, %symbol, %order_id, error = %e,
                                            "撤单失败但订单已不在挂单列表（已成交或已撤），\
                                             合成 Cancelled 清理本地 pending"
                                        );
                                        Self::publish_cancelled(
                                            &income_pubsub,
                                            exchange,
                                            symbol,
                                            order_id,
                                            client_order_id,
                                        )
                                        .await;
                                    }
                                },
                                Err(e2) => {
                                    tracing::error!(
                                        %exchange, %symbol, %order_id,
                                        cancel_error = %e, recheck_error = %e2,
                                        "撤单失败且复查挂单列表也失败，本地 pending 保留，待策略重试"
                                    );
                                }
                            }
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

                    // 币本位 -> 交易所单位。缺 SymbolMeta 时无法算出正确数量：
                    // 若按原值发出，OKX 这类张数计价的所会静默错 contract_size 倍，
                    // 故宁可丢弃并 error（缺 meta 属启动期配置错误，应当刺眼）。
                    let Some(meta) = self.symbol_metas.get(&(order.exchange, order.symbol.clone()))
                    else {
                        let reason = format!(
                            "No SymbolMeta for {}/{}, cannot convert order quantity to exchange unit",
                            order.exchange, order.symbol
                        );
                        tracing::error!(
                            exchange = %order.exchange,
                            symbol = %order.symbol,
                            "{}", reason
                        );
                        self.send_order_error(&order, reason).await;
                        continue;
                    };
                    // 数量下界（取整为 0 / 低于最小下单量）在 from_domain 里拦下：
                    // 这种单发出去必被交易所拒，本地拦截让失败原因可定位，且不白打 REST
                    let exchange_order = match ExchangeOrder::from_domain(order.clone(), meta) {
                        Ok(wire) => wire,
                        Err(reason) => {
                            tracing::error!(
                                exchange = %order.exchange,
                                symbol = %order.symbol,
                                client_order_id = ?order.client_order_id,
                                %reason,
                                "订单未通过出向校验，未发送"
                            );
                            self.send_order_error(&order, reason).await;
                            continue;
                        }
                    };

                    let income_pubsub = self.income_pubsub.clone();
                    tokio::spawn(async move {
                        match client.place_order(exchange_order).await {
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
                                // 按结构化 RejectReason 判断，不再字符串嗅探：
                                // reduce-only 因仓位已平被拒是无害的（平仓目标已达成），降级为 info。
                                match &e {
                                    ExchangeError::OrderRejected(
                                        _,
                                        RejectReason::ReduceOnlyClosed,
                                    ) => {
                                        tracing::info!(
                                            exchange = %order.exchange,
                                            symbol = %order.symbol,
                                            client_order_id = ?order.client_order_id,
                                            "Reduce-only order rejected: position already closed"
                                        );
                                    }
                                    _ => {
                                        tracing::error!(
                                            exchange = %order.exchange,
                                            symbol = %order.symbol,
                                            client_order_id = ?order.client_order_id,
                                            error = %e,
                                            "Failed to place order"
                                        );
                                    }
                                }
                                Self::send_order_error_static(&income_pubsub, &order, e.to_string())
                                    .await;
                            }
                        }
                    });
                }
            }
        }
    }
}

/// 撤单失败复查的判定结果
#[derive(Debug, PartialEq, Eq)]
enum CancelRecheckVerdict {
    /// 订单仍挂在交易所：撤单真失败了，本地 pending 保留是正确的
    StillOpen,
    /// 订单确认已不在挂单列表：已终态，可合成 Cancelled 清理本地 pending
    Gone,
    /// 无法核实身份：列表里存在与本单无从比对的挂单，宁可保留 pending
    Unverifiable,
}

/// 纯判定：撤单失败后，本单是否还挂在交易所上。
///
/// 身份匹配以 `client_order_id` 为主 —— 它是本地 pending 的键、由本引擎生成、四所的
/// `fetch_pending_orders` 都回填；`order_id` 只作辅助且必须非空（IBKR 的推送里它可能
/// 缺失为空串，空串与任何挂单都不相等，若以它为唯一判据，活单会被误判"已终态"）。
///
/// 列表里若存在"两个 id 都无法比对"的挂单（对方无 client_order_id 且本单无 order_id），
/// 那张匿名挂单可能就是本单 —— 判为无法核实，**绝不**合成 Cancelled：误清活单的代价
/// （策略重复下单、双重敞口）远大于误留死单（symbol 冻结，可观测、可重启恢复）。
fn cancel_recheck_verdict(
    open_orders: &[OrderUpdate],
    order_id: &str,
    client_order_id: &str,
) -> CancelRecheckVerdict {
    let matches = |o: &&OrderUpdate| {
        o.client_order_id.as_deref() == Some(client_order_id)
            || (!order_id.is_empty() && o.order_id == order_id)
    };
    if open_orders.iter().any(|o| matches(&o)) {
        return CancelRecheckVerdict::StillOpen;
    }
    let has_anonymous = open_orders
        .iter()
        .any(|o| o.client_order_id.is_none() && order_id.is_empty());
    if has_anonymous {
        CancelRecheckVerdict::Unverifiable
    } else {
        CancelRecheckVerdict::Gone
    }
}

impl OutcomeProcessorActor {
    /// 合成撤单确认回报（撤单成功、或复查确认订单已不在挂单列表时）。
    ///
    /// `client_order_id` 必须是**本地订单号**：SymbolState 的 pending 以它为键移除。
    async fn publish_cancelled(
        income_pubsub: &ActorRef<IncomePubSub>,
        exchange: Exchange,
        symbol: Symbol,
        order_id: crate::domain::OrderId,
        client_order_id: String,
    ) {
        let local_ts = now_ms();
        let update = OrderUpdate {
            order_id,
            client_order_id: Some(client_order_id),
            exchange,
            symbol,
            side: Side::Long, // 撤单事件中 side 无实际意义
            status: OrderStatus::Cancelled,
            price: 0.0,
            reduce_only: false, // 合成的撤单确认，终态不会触发外部单注册
            quantity: 0.0,
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: local_ts,
        };
        if let Err(e) = income_pubsub
            .tell(Publish(IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::OrderUpdate(update),
            }))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish cancel confirmation");
        }
    }

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
            price: 0.0,
            reduce_only: order.reduce_only,
            quantity: 0.0,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open_order(order_id: &str, client_order_id: Option<&str>) -> OrderUpdate {
        OrderUpdate {
            order_id: order_id.to_string(),
            client_order_id: client_order_id.map(str::to_string),
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            side: Side::Long,
            status: OrderStatus::Pending,
            price: 100.0,
            reduce_only: false,
            quantity: 1.0,
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: 1,
        }
    }

    /// client_order_id 是主判据：order_id 为空（IBKR 推送可能缺失）也能认出自己的单
    #[test]
    fn still_open_is_detected_by_client_order_id_even_with_empty_order_id() {
        let open = vec![open_order("777", Some("x-abc"))];
        assert_eq!(
            cancel_recheck_verdict(&open, "", "x-abc"),
            CancelRecheckVerdict::StillOpen,
            "空 order_id 时活单被误判为已终态 —— 会清掉活单的 pending，策略重复下单"
        );
    }

    /// order_id 非空时可作辅助判据（交易所没回填 client_order_id 的场景）
    #[test]
    fn still_open_is_detected_by_order_id_fallback() {
        let open = vec![open_order("777", None)];
        assert_eq!(
            cancel_recheck_verdict(&open, "777", "x-abc"),
            CancelRecheckVerdict::StillOpen
        );
    }

    /// 两个 id 都无法比对的匿名挂单在场时，绝不合成 Cancelled
    #[test]
    fn anonymous_open_order_with_empty_order_id_is_unverifiable() {
        let open = vec![open_order("777", None)];
        assert_eq!(
            cancel_recheck_verdict(&open, "", "x-abc"),
            CancelRecheckVerdict::Unverifiable,
            "匿名挂单可能就是本单，判 Gone 会误清活单"
        );
    }

    /// 列表里全部身份可比且都不匹配，才允许判定已终态
    #[test]
    fn gone_only_when_all_open_orders_are_identifiable_and_none_match() {
        let open = vec![open_order("888", Some("x-other"))];
        assert_eq!(
            cancel_recheck_verdict(&open, "777", "x-abc"),
            CancelRecheckVerdict::Gone
        );
        // 空列表：确认不在
        assert_eq!(
            cancel_recheck_verdict(&[], "", "x-abc"),
            CancelRecheckVerdict::Gone
        );
    }
}

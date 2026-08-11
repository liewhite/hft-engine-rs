//! 实盘出向：OrderGateway（唯一下单出口）+ OutcomeProcessorActor（总线订阅薄壳）。
//!
//! **系统的出向单位边界**：策略与 `StrategyRunner` 产出的订单一律币本位，在此转为
//! [`ExchangeOrder`]（折算 + 按交易所精度取整）后才发给交易所。回测不经过本模块，
//! 因此 `SimState` 拿到的同样是币本位订单 —— 两条路径在策略侧口径一致。

use crate::domain::{
    now_ms, Exchange, ExchangeError, Order, OrderStatus, OrderUpdate, RejectReason, Side, Symbol,
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

// ============================================================================
// OrderGateway —— 唯一的实盘下单出口
// ============================================================================

/// 实盘出向执行器：系统里**唯一**知道"怎么把订单发到交易所"的组件。
///
/// 两个持有方共享同一实例（因此共享同一套 in_flight 判据、失败反馈与 dry_run 语义）：
/// - [`OutcomeProcessorActor`]（总线路径）：策略信号 `tokio::spawn` 执行，不阻塞事件流；
/// - `ManagerActor`（降级平仓，见 `RemoveStrategies`）：`await` 执行，同步拿到结论。
///
/// 此前降级平仓在 manager 里直发 `client.place_order` —— 系统存在第二个下单出口：
/// 没有 in_flight 假警报防护、失败不合成回报、无视 dry_run。收敛为单一出口后，
/// 这些语义对一切下单一致。
pub struct OrderGateway {
    /// 交易所客户端映射
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// Income PubSub（用于发布下单失败/撤单确认事件）
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据：用于把币本位订单折算为交易所下单单位并取整
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// dry-run 模式：只打日志不下单
    dry_run: bool,
    /// 已发出 REST 下单请求、但交易所**还没对它说过任何话**的订单。
    ///
    /// 用于判定一次 REST 失败是否还作数：`OrderStatus::Error` 是本地调用的结论
    /// （只有本模块与模拟柜台会产生它，交易所侧永远不报），而 REST 响应可能比
    /// WS 回报晚得多 —— 实测 IBKR 限流时成交 0.7 秒到、REST 的 503 十秒后才回。
    /// 那时订单早已成交，再发一条"下单失败"就是假警报，还会在账本里留一行假的
    /// error。所以：交易所一旦说过话（任何 OrderUpdate），本地的 REST 结论就作废。
    ///
    /// 由多个并发的下单任务与 `Message<IncomeEvent>`（ack）两处访问，故用 Mutex。
    in_flight: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// [`OrderGateway::place`] 的结构化结论，给**编排方**消费（策略侧的闭环走事件回报，
/// 两者语义不同，不能压扁成一个 bool —— 尤其 [`Self::ExchangeSpoke`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceVerdict {
    /// REST 确认已受理
    Accepted,
    /// dry-run 模式，未发送
    DryRun,
    /// reduce-only 单因仓位已平被拒 —— 平仓语义下目标已达成
    ReduceOnlyAlreadyClosed,
    /// REST 失败，但交易所已经通过回报流对该单表过态，**内容未知** —— 可能是成交、
    /// 挂上，也可能是拒单。策略路径以回报为准（`SymbolState` 会消费它闭环）；编排路径
    /// （降级平仓：executor 已撤、回报无人消费）**不得**据此认定目标达成，须如实上报
    /// 或复核事实 —— 把它折算成"成功"曾是一个 Critical：WS 拒单先到、REST 错误后到时，
    /// 平仓被谎报为完成，敞口无人看管。
    ExchangeSpoke,
}

impl OrderGateway {
    pub fn new(
        clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
        income_pubsub: ActorRef<IncomePubSub>,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        dry_run: bool,
    ) -> Self {
        Self {
            clients,
            income_pubsub,
            symbol_metas,
            dry_run,
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// 下一张单：出向校验（折算 + 取整 + 下界）→ 登记 in_flight → REST → 失败反馈。
    ///
    /// # 返回值是给**编排方**的结论（策略侧的闭环走事件回报）
    ///
    /// - `Ok(verdict)`：见 [`PlaceVerdict`] 各变体 —— 特别地，`ExchangeSpoke` 不是成功，
    ///   只是"REST 的失败结论已被交易所回报作废"。
    /// - `Err`：这张单确定没挂上，且原因作数 —— 调用方（降级平仓）据此如实计入"未完成"。
    ///
    /// 一切失败路径都已通过 [`Self::send_order_error`] 反馈为 `OrderUpdate(Error)`，
    /// 总线路径 spawn 后忽略返回值不会丢失信息。
    pub async fn place(&self, order: Order, comment: &str) -> Result<PlaceVerdict, String> {
        let Some(client) = self.clients.get(&order.exchange).cloned() else {
            let reason = format!("No client found for exchange {}", order.exchange);
            tracing::error!(exchange = %order.exchange, "{}", reason);
            self.send_order_error(&order, reason.clone()).await;
            return Err(reason);
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
            return Ok(PlaceVerdict::DryRun);
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
        let Some(meta) = self.symbol_metas.get(&(order.exchange, order.symbol.clone())) else {
            let reason = format!(
                "No SymbolMeta for {}/{}, cannot convert order quantity to exchange unit",
                order.exchange, order.symbol
            );
            tracing::error!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                "{}", reason
            );
            self.send_order_error(&order, reason.clone()).await;
            return Err(reason);
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
                self.send_order_error(&order, reason.clone()).await;
                return Err(reason);
            }
        };

        // 登记为"在飞"：交易所还没对它说过任何话。任何一条 OrderUpdate
        // 到达都会把它摘掉（见 [`Self::ack`]），此后 REST 的结论不再作数。
        self.in_flight
            .lock()
            .expect("in_flight 锁被毒化")
            .insert(order.client_order_id.clone());

        match client.place_order(exchange_order).await {
            Ok(order_id) => {
                // REST 已给出结论，此后不再需要"在飞"登记
                self.in_flight
                    .lock()
                    .expect("in_flight 锁被毒化")
                    .remove(&order.client_order_id);
                tracing::info!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    order_id = %order_id,
                    client_order_id = ?order.client_order_id,
                    "Order placed successfully"
                );
                Ok(PlaceVerdict::Accepted)
            }
            Err(e) => {
                // 按结构化 RejectReason 判断，不再字符串嗅探：
                // reduce-only 因仓位已平被拒是无害的（平仓目标已达成），降级为 info。
                let reduce_only_closed = matches!(
                    &e,
                    ExchangeError::OrderRejected(_, RejectReason::ReduceOnlyClosed)
                );
                if reduce_only_closed {
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
                        error = %e,
                        "Failed to place order"
                    );
                }
                // 交易所已经对这张单说过话（成交/挂上/被拒），本地这条
                // REST 结论就作废：再发一条 Error 是假警报，还会在账本
                // 里留一行假的 error。remove 返回 false 即"已被摘掉"。
                let still_in_flight = self
                    .in_flight
                    .lock()
                    .expect("in_flight 锁被毒化")
                    .remove(&order.client_order_id);
                if still_in_flight {
                    self.send_order_error(&order, e.to_string()).await;
                } else {
                    tracing::info!(
                        exchange = %order.exchange,
                        symbol = %order.symbol,
                        client_order_id = ?order.client_order_id,
                        error = %e,
                        "REST 下单失败，但交易所已回报过该单 —— 以交易所回报为准，不再上报失败"
                    );
                }
                // 编排方视角的三种分道（不可压扁，见 PlaceVerdict）：
                // 仓位已平 = 目标达成；交易所已表态 = 内容未知、由调用方裁断；其余 = 真失败
                if reduce_only_closed {
                    Ok(PlaceVerdict::ReduceOnlyAlreadyClosed)
                } else if !still_in_flight {
                    Ok(PlaceVerdict::ExchangeSpoke)
                } else {
                    Err(e.to_string())
                }
            }
        }
    }

    /// 撤一张单：REST 撤单 → 成功合成 Cancelled 回报；失败用挂单列表复查裁定。
    ///
    /// 返回 `Ok` = 该单已确认不在交易所（撤掉或本就终态）；`Err` = 单可能仍挂着
    /// （撤单真失败 / 无法核实），本地 pending 保留，待策略重试。
    pub async fn cancel(
        &self,
        exchange: Exchange,
        symbol: Symbol,
        order_id: crate::domain::OrderId,
        client_order_id: String,
    ) -> Result<(), String> {
        let Some(client) = self.clients.get(&exchange).cloned() else {
            let reason = format!("No client found for cancel_order on {exchange}");
            tracing::error!(%exchange, "{}", reason);
            return Err(reason);
        };

        if self.dry_run {
            tracing::warn!(%exchange, %symbol, %order_id, "[DRY-RUN] CancelOrder NOT sent");
            return Ok(());
        }

        tracing::info!(%exchange, %symbol, %order_id, "Cancelling order");
        match client.cancel_order(&symbol, &order_id).await {
            Ok(()) => {
                tracing::info!(%exchange, %symbol, %order_id, "Order cancelled successfully");
                // 撤单成功，合成 Cancelled 回报让 SymbolState 移除 pending。
                // 必须携带真实的 client_order_id —— pending 以它为键，填错
                // （此前填的是交易所 order_id）会让撤掉的单永远留在本地。
                self.publish_cancelled(exchange, symbol, order_id, client_order_id)
                    .await;
                Ok(())
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
                    Ok(open_orders) => {
                        match cancel_recheck_verdict(&open_orders, &order_id, &client_order_id) {
                            CancelRecheckVerdict::StillOpen => {
                                tracing::error!(
                                    %exchange, %symbol, %order_id, error = %e,
                                    "撤单失败且订单仍挂在交易所，保留本地 pending，待策略重试"
                                );
                                Err(format!("撤单失败且订单仍挂在交易所: {e}"))
                            }
                            CancelRecheckVerdict::Unverifiable => {
                                tracing::error!(
                                    %exchange, %symbol, %order_id, error = %e,
                                    "撤单失败且挂单列表里存在无法比对身份的挂单，\
                                     保留本地 pending（误清活单比误留死单危险）"
                                );
                                Err(format!("撤单失败且无法核实订单是否仍挂着: {e}"))
                            }
                            CancelRecheckVerdict::Gone => {
                                tracing::warn!(
                                    %exchange, %symbol, %order_id, error = %e,
                                    "撤单失败但订单已不在挂单列表（已成交或已撤），\
                                     合成 Cancelled 清理本地 pending"
                                );
                                self.publish_cancelled(exchange, symbol, order_id, client_order_id)
                                    .await;
                                Ok(())
                            }
                        }
                    }
                    Err(e2) => {
                        tracing::error!(
                            %exchange, %symbol, %order_id,
                            cancel_error = %e, recheck_error = %e2,
                            "撤单失败且复查挂单列表也失败，本地 pending 保留，待策略重试"
                        );
                        Err(format!("撤单失败（{e}）且复查也失败（{e2}）"))
                    }
                }
            }
        }
    }

    /// 交易所对某单说过话：作废本地的 REST 结论（收到该单任何 OrderUpdate 时调用）。
    pub fn ack(&self, client_order_id: &str) {
        self.in_flight
            .lock()
            .expect("in_flight 锁被毒化")
            .remove(client_order_id);
    }

    /// 合成撤单确认回报（撤单成功、或复查确认订单已不在挂单列表时）。
    ///
    /// `client_order_id` 必须是**本地订单号**：SymbolState 的 pending 以它为键移除。
    async fn publish_cancelled(
        &self,
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
            quantity: 0.0,
        };
        if let Err(e) = self
            .income_pubsub
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

    /// 发送订单错误事件（策略侧的失败闭环：Error 终态清 pending）
    async fn send_order_error(&self, order: &Order, reason: String) {
        let local_ts = now_ms();
        let update = OrderUpdate {
            order_id: String::new(),
            client_order_id: Some(order.client_order_id.clone()),
            exchange: order.exchange,
            symbol: order.symbol.clone(),
            side: order.side,
            status: OrderStatus::Error { reason },
            // 如实带上委托量（与柜台的拒单回报口径一致），不填 0
            quantity: order.quantity,
        };

        if let Err(e) = self
            .income_pubsub
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

// ============================================================================
// OutcomeProcessorActor —— 总线订阅薄壳
// ============================================================================

/// OutcomeProcessorActor - 订阅 OutcomePubSub，把实盘账户的策略信号交给 [`OrderGateway`]。
pub struct OutcomeProcessorActor {
    gateway: Arc<OrderGateway>,
}

impl Actor for OutcomeProcessorActor {
    type Args = Arc<OrderGateway>;
    type Error = Infallible;

    async fn on_start(gateway: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        if gateway.dry_run {
            tracing::warn!("OutcomeProcessorActor started in DRY-RUN mode (orders will NOT be placed)");
        } else {
            tracing::info!("OutcomeProcessorActor started");
        }
        Ok(Self { gateway })
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
        // 下单/撤单一律 `tokio::spawn` 异步执行，**有意为之**：REST 往返可能上百 ms，
        // 若在 handler 内 await 会阻塞本 actor 的邮箱，进而拖垮整条 income/outcome 事件流
        // （行情、成交回报全部积压）。
        //
        // 已知取舍（当前不处理，靠上层机制兜底）：
        // - 同一 symbol 的多笔下单/撤单可能乱序到达交易所——策略层的 rebalance /
        //   reduce-only 会在后续事件中纠正净敞口，不依赖单条请求的到达序。
        // - 优雅停机时 `on_stop` 不等待 in-flight 请求，理论上存在"引擎已停但请求仍在途"
        //   的窗口；接受此风险以换取事件流不阻塞。
        // - spawned task 内的失败已由 gateway 反馈为 OrderUpdate(Error/Cancelled)，
        //   不会静默丢失 —— 返回值只服务需要同步结论的编排方（降级平仓），此处忽略。
        match tagged.event {
            // 自定义事件与交易所无关：它已随 Outcome 总线到达外部订阅者，本处理器只管下单/撤单
            OutcomeEvent::Emit(_) => {}
            OutcomeEvent::CancelOrder {
                exchange,
                symbol,
                order_id,
                client_order_id,
            } => {
                let gateway = self.gateway.clone();
                tokio::spawn(async move {
                    let _ = gateway.cancel(exchange, symbol, order_id, client_order_id).await;
                });
            }
            OutcomeEvent::PlaceOrders { orders, comment } => {
                // 关联订单独立并行下单：IOC 订单本身接受部分成交，
                // 敞口由策略层 rebalance 机制兜底修正。
                for order in orders {
                    let gateway = self.gateway.clone();
                    let comment = comment.clone();
                    tokio::spawn(async move {
                        let _ = gateway.place(order, &comment).await;
                    });
                }
            }
        }
    }
}

/// 交易所回报入口：**只用来作废本地的 REST 结论**。
///
/// 经 `SubscribeFilter` 只订阅 `OrderUpdate`，不吃行情洪水。收到任何一条回报就把该单
/// 从 `in_flight` 摘掉 —— 交易所说过话之后，REST 那条迟到的失败结论不再作数。
///
/// 本处理器**不**据此维护订单状态：订单状态的单一出处是 `SymbolState`，这里只做
/// "谁先说话"的判定。
impl Message<IncomeEvent> for OutcomeProcessorActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        let ExchangeEventData::OrderUpdate(update) = &msg.data else {
            return;
        };
        let Some(client_order_id) = &update.client_order_id else {
            return;
        };
        self.gateway.ack(client_order_id);
    }
}

// ============================================================================
// 撤单失败复查
// ============================================================================

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
            quantity: 1.0,
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

    // ==================== 迟到的 REST 失败结论 ====================
    //
    // 这套判定全靠时序，所以直接对 in_flight 这个判据本身建模：
    // 「登记 → 交易所说话 → REST 失败」与「登记 → REST 失败」两条路必须分道。

    use std::collections::HashSet;
    use std::sync::Mutex;

    /// 复刻 gateway 里的两步：交易所回报摘除、REST 失败时判是否还作数
    fn ack(in_flight: &Mutex<HashSet<String>>, cid: &str) {
        in_flight.lock().unwrap().remove(cid);
    }
    fn rest_failed_should_report(in_flight: &Mutex<HashSet<String>>, cid: &str) -> bool {
        in_flight.lock().unwrap().remove(cid)
    }

    /// 交易所先回报、REST 后失败 → **不得**上报失败。
    ///
    /// 这正是 IBKR 限流时的真实时序：成交 0.7 秒到，REST 的 503 十秒后才回。
    /// 那时单子早成交了，再发一条"下单失败"是假警报，还会往账本里写一行假 error
    /// （实测一次事故里 754 条 error 有 751 条属于此类）。
    #[test]
    fn late_rest_failure_is_dropped_when_exchange_already_reported() {
        let in_flight = Mutex::new(HashSet::new());
        in_flight.lock().unwrap().insert("c-1".to_string());

        ack(&in_flight, "c-1"); // WS 回报先到
        assert!(
            !rest_failed_should_report(&in_flight, "c-1"),
            "交易所已回报过，迟到的 REST 失败不该再上报"
        );
    }

    /// 交易所没说过话 → REST 失败照常上报（这才是真失败）
    #[test]
    fn rest_failure_is_reported_when_exchange_never_spoke() {
        let in_flight = Mutex::new(HashSet::new());
        in_flight.lock().unwrap().insert("c-2".to_string());

        assert!(
            rest_failed_should_report(&in_flight, "c-2"),
            "交易所从未回报，REST 失败就是结论，必须上报"
        );
    }

    /// 判据只作数一次：同一单不会因为重复回报而被上报两次
    #[test]
    fn a_single_order_is_judged_at_most_once() {
        let in_flight = Mutex::new(HashSet::new());
        in_flight.lock().unwrap().insert("c-3".to_string());

        assert!(rest_failed_should_report(&in_flight, "c-3"));
        assert!(
            !rest_failed_should_report(&in_flight, "c-3"),
            "已判过的单不该再次上报"
        );
    }
}

/// 对 [`OrderGateway::place`] 的返回值契约直测（不是复刻实现）。
///
/// 这份契约被降级平仓消费：把 `ExchangeSpoke` 折算成"成功"曾是一个 Critical ——
/// WS 拒单先到、REST 错误后到时，平仓被谎报为完成、敞口无人看管。
#[cfg(test)]
mod place_verdict_tests {
    use super::*;
    use crate::domain::{
        AccountInfo, OrderId, OrderType, Position, RejectReason, Side, TimeInForce,
    };
    use crate::exchange::utils::StepFormatter;
    use kameo::actor::Spawn;
    use kameo::mailbox;
    use kameo_actors::pubsub::Subscribe;
    use kameo_actors::DeliveryStrategy;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";
    const CID: &str = "x-test-1";

    /// place_order 的可编程行为：测试注入结果；`gate` 存在时先通知已进入、再等放行
    /// （用于在 REST 在途期间模拟"WS 回报先到"）。
    struct FakeClient {
        result: Mutex<Option<Result<OrderId, ExchangeError>>>,
        gate: Option<(mpsc::Sender<()>, tokio::sync::Mutex<mpsc::Receiver<()>>)>,
    }

    #[async_trait::async_trait]
    impl ExchangeClient for FakeClient {
        fn exchange(&self) -> Exchange {
            EX
        }
        async fn place_order(&self, _order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
            if let Some((entered_tx, proceed_rx)) = &self.gate {
                entered_tx.send(()).await.expect("通知已进入 place_order");
                proceed_rx.lock().await.recv().await.expect("等待放行");
            }
            self.result.lock().unwrap().take().expect("结果只取一次")
        }
        async fn cancel_order(&self, _s: &Symbol, _o: &OrderId) -> Result<(), ExchangeError> {
            unreachable!("place 契约测试不撤单")
        }
        async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
            unreachable!()
        }
        async fn fetch_symbol_meta(&self, _s: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
            unreachable!()
        }
        async fn fetch_pending_orders(&self, _s: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError> {
            unreachable!()
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!()
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            unreachable!()
        }
    }

    /// 收集 income 总线上的回报，供断言"是否发了 Error 回报"
    struct Sink(Arc<Mutex<Vec<IncomeEvent>>>);
    impl Actor for Sink {
        type Args = Arc<Mutex<Vec<IncomeEvent>>>;
        type Error = Infallible;
        async fn on_start(a: Self::Args, _r: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self(a))
        }
    }
    impl Message<IncomeEvent> for Sink {
        type Reply = ();
        async fn handle(&mut self, m: IncomeEvent, _c: &mut Context<Self, Self::Reply>) {
            self.0.lock().unwrap().push(m);
        }
    }

    fn order() -> Order {
        Order {
            id: String::new(),
            exchange: EX,
            symbol: SYM.to_string(),
            side: Side::Short,
            order_type: OrderType::Limit {
                price: 100.0,
                tif: TimeInForce::GTC,
            },
            quantity: 1.0,
            reduce_only: true,
            client_order_id: CID.to_string(),
        }
    }

    fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        Arc::new(HashMap::from([(
            (EX, SYM.to_string()),
            SymbolMeta {
                exchange: EX,
                symbol: SYM.to_string(),
                price_formatter: Arc::new(StepFormatter::new(0.1)),
                size_step: 0.001,
                min_order_size: 0.001,
                contract_size: 1.0,
            },
        )]))
    }

    struct Harness {
        gateway: Arc<OrderGateway>,
        events: Arc<Mutex<Vec<IncomeEvent>>>,
    }

    impl Harness {
        async fn new(client: FakeClient) -> Self {
            let pubsub = IncomePubSub::spawn_with_mailbox(
                IncomePubSub::new(DeliveryStrategy::Guaranteed),
                mailbox::unbounded(),
            );
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Sink::spawn_with_mailbox(events.clone(), mailbox::unbounded());
            pubsub.tell(Subscribe(sink)).send().await.unwrap();
            let clients: HashMap<Exchange, Arc<dyn ExchangeClient>> =
                HashMap::from([(EX, Arc::new(client) as Arc<dyn ExchangeClient>)]);
            let gateway = Arc::new(OrderGateway::new(clients, pubsub, metas(), false));
            Self { gateway, events }
        }

        /// 是否发出了 Error 终态回报（策略侧失败闭环）
        async fn error_reported(&self) -> bool {
            // 给总线转发留时间
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.events.lock().unwrap().iter().any(|e| {
                matches!(
                    &e.data,
                    ExchangeEventData::OrderUpdate(u)
                        if matches!(u.status, OrderStatus::Error { .. })
                )
            })
        }
    }

    /// REST 受理 → Accepted，无失败回报
    #[tokio::test]
    async fn rest_accept_is_accepted() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Ok("ex-1".to_string()))),
            gate: None,
        })
        .await;
        let verdict = h.gateway.place(order(), "t").await;
        assert_eq!(verdict, Ok(PlaceVerdict::Accepted));
        assert!(!h.error_reported().await, "受理成功不该发失败回报");
    }

    /// reduce-only 因仓位已平被拒 → 编排方得"目标已达成"，策略侧照常收 Error 闭环
    #[tokio::test]
    async fn reduce_only_closed_is_already_closed() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(ExchangeError::OrderRejected(
                EX,
                RejectReason::ReduceOnlyClosed,
            )))),
            gate: None,
        })
        .await;
        let verdict = h.gateway.place(order(), "t").await;
        assert_eq!(verdict, Ok(PlaceVerdict::ReduceOnlyAlreadyClosed));
        assert!(h.error_reported().await, "策略侧仍需 Error 回报清 pending");
    }

    /// 纯 REST 失败（交易所从未回报）→ Err，且发 Error 回报
    #[tokio::test]
    async fn pure_rest_failure_is_err_with_feedback() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(ExchangeError::Other("boom".to_string())))),
            gate: None,
        })
        .await;
        let verdict = h.gateway.place(order(), "t").await;
        assert!(verdict.is_err(), "真失败必须是 Err: {verdict:?}");
        assert!(h.error_reported().await, "真失败必须反馈 Error 回报");
    }

    /// **Critical 回归防线**：REST 在途期间交易所已回报（内容未知，可能是拒单）→
    /// 结论必须是 `ExchangeSpoke` 而不是"成功"，且不再发假的 Error 回报。
    /// 降级平仓据此把该腿计入"未完成"，绝不谎报已关闭。
    #[tokio::test]
    async fn ws_report_during_rest_flight_is_exchange_spoke_not_success() {
        let (entered_tx, mut entered_rx) = mpsc::channel(1);
        let (proceed_tx, proceed_rx) = mpsc::channel(1);
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(ExchangeError::Other("late 503".to_string())))),
            gate: Some((entered_tx, tokio::sync::Mutex::new(proceed_rx))),
        })
        .await;

        let task = tokio::spawn({
            let gateway = h.gateway.clone();
            async move { gateway.place(order(), "t").await }
        });
        // REST 在途：交易所的回报（此处可能是 Rejected！）先到，作废本地结论
        entered_rx.recv().await.expect("place_order 已进入");
        h.gateway.ack(CID);
        proceed_tx.send(()).await.expect("放行 REST 返回失败");

        let verdict = task.await.expect("task");
        assert_eq!(
            verdict,
            Ok(PlaceVerdict::ExchangeSpoke),
            "交易所已表态但内容未知 —— 不是成功，也不是可上报的失败"
        );
        assert!(
            !h.error_reported().await,
            "交易所已回报过，迟到的 REST 失败不该再发假 Error"
        );
    }
}

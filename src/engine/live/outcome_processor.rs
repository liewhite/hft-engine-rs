//! 实盘出向：OrderGateway（唯一下单出口）+ OutcomeProcessor（总线订阅薄壳）。
//!
//! **系统的出向单位边界**：策略与 `StrategyRunner` 产出的订单一律币本位，在此转为
//! [`ExchangeOrder`]（折算 + 按交易所精度取整）后才发给交易所。回测不经过本模块，
//! 因此 `SimState` 拿到的同样是币本位订单 —— 两条路径在策略侧口径一致。

use crate::domain::{
    now_ms, Exchange, Order, OrderStatus, OrderUpdate, PlaceError, RejectReason, Side, Symbol,
    SymbolMeta,
};
use crate::exchange::{AccountClient, ExchangeOrder};
use crate::domain::AccountId;
use crate::messaging::{AccountBus, AccountData, AccountEvent, SymbolAccountData};
use crate::strategy::OutcomeEvent;
use crate::tasks::{FailHandle, Group, Inbox, Scope, StopFlag, Task};
use std::collections::HashMap;
use std::sync::Arc;

use super::{outlet_for, to_live_outlet, AccountOutcome, OutcomeBus, Outlet};

// ============================================================================
// OrderGateway —— 唯一的实盘下单出口
// ============================================================================

/// 实盘出向执行器：系统里**唯一**知道"怎么把订单发到交易所"的组件。
///
/// 由 [`OutcomeProcessor`] 独占：策略信号各起一个子任务执行，不阻塞事件流。系统里不存在第二个下单
/// 出口，in_flight 假警报防护、失败合成回报、dry_run 语义对一切下单一致。
pub struct OrderGateway {
    /// 各所的账户私有能力。键集 = 配了凭证的所 —— 不在表里就是"该所只接公共行情"，
    /// 下单请求会被拒绝并合成失败回报，而不是静默丢弃。
    accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 账户事件总线（用于发布下单失败/撤单确认回报，标 Live）
    account_bus: AccountBus,
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
    /// 由多个并发的下单任务与交易所回报（[`Self::ack`]）两处访问，故用 Mutex。
    in_flight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// 下单结果不确定时的停机开关：由 manager 构造时交入它自己的 `scope.fail_handle()`。
    /// 触发 = manager 带错退出、整棵树停机，`run_engine` 以错误返回、进程退出 ——
    /// 与持仓对账确认漂移时同一条停机路径。
    halt: FailHandle,
    /// 整机是否已开始停机（任何原因：下单结果不确定、持仓漂移、外部停机信号……）。
    /// 一旦停机就不再发新单：停机收尾期间策略可能还在出信号，这些单挂到交易所上、进程退出后
    /// 无人看管（#710）。撤单不受影响。
    machine_stopping: StopFlag,
}

/// [`OrderGateway::place`] 的结构化结论。策略侧的闭环走事件回报，不读它；它把一次下单落在
/// 哪种结果如实说出来（不压扁成一个 bool —— 尤其 [`Self::ExchangeSpoke`]），单测据此钉住三种结果的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceVerdict {
    /// REST 确认已受理
    Accepted,
    /// dry-run 模式，未发送
    DryRun,
    /// reduce-only 单因仓位已平被拒 —— 平仓语义下目标已达成
    ReduceOnlyAlreadyClosed,
    /// REST 失败，但交易所已经通过回报流对该单表过态，**内容未知** —— 可能是成交、
    /// 挂上，也可能是拒单。策略路径以回报为准（`SymbolState` 会消费它闭环）；若将来有
    /// 同步等结论、又没人消费回报的调用方，**不得**据此认定目标达成 —— WS 拒单先到、
    /// REST 错误后到时，那会把没挂上的单当成功。
    ExchangeSpoke,
}

impl OrderGateway {
    /// `halt` / `machine_stopping`：manager 的 `scope.fail_handle()` 与 `scope.machine_stop_flag()`。
    /// 前者触发会立刻让后者变为「在停机」（任何节点出错都同步请求根停机），故结果不确定
    /// 之后的新单由同一道闸拦下。
    pub fn new(
        accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
        account_bus: AccountBus,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        dry_run: bool,
        halt: FailHandle,
        machine_stopping: StopFlag,
    ) -> Self {
        Self {
            accounts,
            account_bus,
            symbol_metas,
            dry_run,
            in_flight: std::sync::Mutex::new(std::collections::HashSet::new()),
            halt,
            machine_stopping,
        }
    }

    /// 下一张单：出向校验（折算 + 取整 + 下界）→ 登记 in_flight → REST → 按三种结果处置。
    ///
    /// # 三种结果（见 [`PlaceError`]）
    ///
    /// - 成功：交易所回报流接手。
    /// - 被拒绝（单子确定没挂上）：反馈 `OrderUpdate(Error)` 给策略，策略清掉这张挂单、照常再下。
    /// - 错误（结果不确定，单子可能已挂上或已成交）：**不**反馈 Error —— 那会让策略以为单子
    ///   没了，再挂一张、再补一次腿。若交易所回报流已经认领了这张单（in_flight 已被摘掉），
    ///   策略手里就有真实状态，以回报为准、继续运行；否则框架无从得知真相，停机交人工核对
    ///   （经构造时传入的 `FailHandle` 停掉 manager）。
    ///
    /// 整机一旦开始停机（不论什么原因），新单一律不发、直接返回 `Err`，也不反馈 Error 回报
    /// （策略正在停，没人再按它重挂）。
    ///
    /// # 返回值（策略侧的闭环走事件回报，不依赖它）
    ///
    /// - `Ok(verdict)`：见 [`PlaceVerdict`] 各变体 —— 特别地，`ExchangeSpoke` 不是成功，
    ///   只是"REST 的结论已被交易所回报作废"。
    /// - `Err`：这张单没有成功挂上（被拒绝、未发出、引擎已在停机），或结果不确定、引擎因此停机。
    pub async fn place(&self, order: Order, comment: &str) -> Result<PlaceVerdict, String> {
        if self.machine_stopping.is_stopping() {
            let reason = "引擎正在停机，不再发新单".to_string();
            tracing::warn!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                client_order_id = ?order.client_order_id,
                "{}", reason
            );
            return Err(reason);
        }

        let Some(client) = self.accounts.get(&order.exchange).cloned() else {
            let reason = format!("{} 没有账户（未配置凭证），无法下单", order.exchange);
            tracing::error!(exchange = %order.exchange, "{}", reason);
            self.send_order_error(&order, reason.clone());
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
            self.send_order_error(&order, reason.clone());
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
                self.send_order_error(&order, reason.clone());
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
                // 交易所已经对这张单说过话（成交/挂上/被拒），本地这条 REST 结论就作废：
                // 状态以回报为准（策略侧 SymbolState 消费它闭环）。remove 返回 false 即"已被摘掉"。
                let exchange_spoke = !self
                    .in_flight
                    .lock()
                    .expect("in_flight 锁被毒化")
                    .remove(&order.client_order_id);
                if exchange_spoke {
                    tracing::warn!(
                        exchange = %order.exchange,
                        symbol = %order.symbol,
                        client_order_id = ?order.client_order_id,
                        error = %e,
                        "REST 下单失败，但交易所已回报过该单 —— 以交易所回报为准，不再上报失败"
                    );
                    return Ok(PlaceVerdict::ExchangeSpoke);
                }
                match e {
                    PlaceError::Rejected(reason) => {
                        // reduce-only 因仓位已平被拒：平仓目标已达成，对编排方不是失败
                        let already_closed = reason == RejectReason::ReduceOnlyClosed;
                        let reason = self.rejected(&order, reason);
                        if already_closed {
                            Ok(PlaceVerdict::ReduceOnlyAlreadyClosed)
                        } else {
                            Err(reason)
                        }
                    }
                    PlaceError::Indeterminate(detail) => Err(self.halt_on_indeterminate(&order, &detail)),
                }
            }
        }
    }

    /// 被拒绝：单子确定没挂上。反馈 Error 给策略清掉挂单（reduce-only 已平也一样要清），
    /// 返回拒单原因。reduce-only 因仓位已平被拒是无害的，只记 info。
    fn rejected(&self, order: &Order, reason: RejectReason) -> String {
        if reason == RejectReason::ReduceOnlyClosed {
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
                %reason,
                "Order rejected"
            );
        }
        let reason = PlaceError::Rejected(reason).to_string();
        self.send_order_error(order, reason.clone());
        reason
    }

    /// 结果不确定且交易所回报流没认领：停机。不反馈 Error（那会让策略以为单子没了）。
    fn halt_on_indeterminate(&self, order: &Order, detail: &str) -> String {
        tracing::error!(
            exchange = %order.exchange,
            symbol = %order.symbol,
            side = %order.side,
            order_type = ?order.order_type,
            quantity = order.quantity,
            client_order_id = ?order.client_order_id,
            %detail,
            "下单结果不确定（单子可能已挂上或已成交），停机交人工核对挂单与持仓"
        );
        self.halt.fail(anyhow::anyhow!(
            "下单结果不确定（{} {} client_order_id={}），停机交人工核对挂单与持仓: {detail}",
            order.exchange,
            order.symbol,
            order.client_order_id
        ));
        format!("下单结果不确定，引擎停机: {detail}")
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
        let Some(client) = self.accounts.get(&exchange).cloned() else {
            let reason = format!("{exchange} 没有账户（未配置凭证），无法撤单");
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
                self.publish_cancelled(exchange, symbol, order_id, client_order_id);
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
                                self.publish_cancelled(exchange, symbol, order_id, client_order_id);
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
    fn publish_cancelled(
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
        self.account_bus.publish(AccountEvent {
            account: AccountId::Live,
            exchange_ts: local_ts,
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::OrderUpdate(update)),
        });
    }

    /// 发送订单错误事件（策略侧的失败闭环：Error 终态清 pending）
    fn send_order_error(&self, order: &Order, reason: String) {
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
        self.account_bus.publish(AccountEvent {
            account: AccountId::Live,
            exchange_ts: local_ts, // 本地错误，没有交易所时间戳
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::OrderUpdate(update)),
        });
    }
}

// ============================================================================
// OutcomeProcessor —— 总线订阅薄壳
// ============================================================================

/// 收件箱消息：两条总线的订阅都落到这里
enum Msg {
    /// 出向总线上归实盘出口的策略信号（订阅过滤：[`to_live_outlet`]）
    Outcome(AccountOutcome),
    /// 账户总线上的订单回报（订阅过滤：[`is_order_update`]）
    OrderUpdate(AccountEvent),
}

/// 账户总线的订阅过滤：只要 `OrderUpdate`，不吃全量账户事件
fn is_order_update(e: &AccountEvent) -> bool {
    matches!(e.data, AccountData::Symbol(SymbolAccountData::OrderUpdate(_)))
}

/// 订阅出向总线，把实盘账户的策略信号交给 [`OrderGateway`]。
pub struct OutcomeProcessor {
    gateway: Arc<OrderGateway>,
    inbox: Inbox<Msg>,
    /// 在途的下单/撤单请求，每张单一个子任务（见 [`Self::on_outcome`]）
    requests: Group,
}

impl OutcomeProcessor {
    /// 在 `group` 下起实盘出口，等它订阅完成。
    ///
    /// 订阅：
    /// - `outcome_bus`，过滤 [`to_live_outlet`]：只认领实盘账户的信号（与本地柜台的
    ///   `to_paper_outlet` 成对，判据只有 `outlet_for` 一处）；
    /// - `account_bus`，只订 `OrderUpdate`：交易所回报用来作废迟到的 REST 失败结论
    ///   （见 `OrderGateway::in_flight`）。
    pub async fn spawn(
        group: &Group,
        gateway: Arc<OrderGateway>,
        outcome_bus: &OutcomeBus,
        account_bus: &AccountBus,
    ) -> anyhow::Result<()> {
        let (outcome_bus, account_bus) = (outcome_bus.clone(), account_bus.clone());
        group
            .spawn("OutcomeProcessor", move |scope: Scope| async move {
                let (_addr, mut inbox) = scope.inbox::<Msg>();
                inbox.subscribe(&outcome_bus, to_live_outlet, Msg::Outcome);
                inbox.subscribe(&account_bus, is_order_update, Msg::OrderUpdate);
                let requests = scope.group("requests");
                if gateway.dry_run {
                    tracing::warn!("OutcomeProcessor started in DRY-RUN mode (orders will NOT be placed)");
                } else {
                    tracing::info!("OutcomeProcessor started");
                }
                Ok(Self { gateway, inbox, requests })
            })
            .await?;
        Ok(())
    }

    fn on_outcome(&self, tagged: AccountOutcome) -> anyhow::Result<()> {
        // 归属由订阅过滤器保证（见 [`Self::spawn`]）。这里再验一次**不是**重新分发：
        // 判据仍是同一个 `outlet_for`，新增账户类型时它编译失败，不存在"两处各判各的"
        // 那种静默漏改。留这道是因为投递路径本身没法在类型上收死（订阅过滤是运行期的），
        // 而装配写错时"响亮失败"远好过静默双执行 —— 品味 1.1 的第二级。
        if outlet_for(&tagged.account) != Outlet::Live {
            tracing::error!(
                account = %tagged.account,
                "非实盘账户的信号到达实盘出口 —— 订阅过滤器装配错误，丢弃"
            );
            return Ok(());
        }
        // 下单/撤单一律各起一个子任务异步执行，**有意为之**：REST 往返可能上百 ms，
        // 若在收件循环里 await，后面的信号与交易所回报（ack）全部排队等它。
        //
        // 子任务挂在本组件的 `requests` 组下而不是游离的 `tokio::spawn`：
        // - 停机时本组件先排空收件箱（排空期间起的单也挂进来），框架再等全部在途请求返回
        //   才算停完 —— 不再有"引擎已停但请求仍在途"的窗口（REST 有自己的超时，根的总期限兜底）；
        // - 请求任务 panic 会作为错误上冒、整机停机，不会被吞掉。
        //
        // 已知取舍（当前不处理，靠上层机制兜底）：同一 symbol 的多笔下单/撤单可能乱序
        // 到达交易所 —— 策略层的 rebalance / reduce-only 会在后续事件中纠正净敞口，不依赖
        // 单条请求的到达序。
        //
        // 子任务内的结论不靠返回值传递：拒单由 gateway 反馈为 OrderUpdate(Error)、撤单反馈为
        // Cancelled；下单结果不确定时 gateway 自己停机（不反馈 Error，见 `OrderGateway::place`）。
        // 返回值在此忽略。
        match tagged.event {
            // 自定义事件与交易所无关：它已随出向总线到达外部订阅者，本处理器只管下单/撤单
            OutcomeEvent::Emit(_) => {}
            OutcomeEvent::CancelOrder {
                exchange,
                symbol,
                order_id,
                client_order_id,
            } => {
                let gateway = self.gateway.clone();
                self.submit(format!("cancel:{exchange}:{order_id}"), async move {
                    let _ = gateway.cancel(exchange, symbol, order_id, client_order_id).await;
                })?;
            }
            OutcomeEvent::PlaceOrders { orders, comment } => {
                // 关联订单独立并行下单：IOC 订单本身接受部分成交，
                // 敞口由策略层 rebalance 机制兜底修正。
                for order in orders {
                    let gateway = self.gateway.clone();
                    let comment = comment.clone();
                    let name = format!("place:{}", order.client_order_id);
                    self.submit(name, async move {
                        let _ = gateway.place(order, &comment).await;
                    })?;
                }
            }
        }
        Ok(())
    }

    /// 把一次请求挂到 `requests` 组下执行。
    fn submit(
        &self,
        name: String,
        request: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        self.requests
            .spawn_fn(name, |scope| async move {
                request.await;
                // 做完即收工：没被要求停止就返回 Ok 会被框架当成出错
                scope.request_stop();
                Ok(())
            })
            // 组只在本组件 run 返回后才关闭，此时不会再有信号要发；真发生了就是一张单没发出去
            .map_err(|e| anyhow::anyhow!("下单/撤单请求无法执行（不该发生）: {e}"))?;
        Ok(())
    }

    /// 交易所回报入口：**只用来作废本地的 REST 结论**。
    ///
    /// 收到任何一条回报就把该单从 `in_flight` 摘掉 —— 交易所说过话之后，REST 那条迟到的
    /// 失败结论不再作数。本处理器**不**据此维护订单状态：订单状态的单一出处是
    /// `SymbolState`，这里只做"谁先说话"的判定。
    fn on_order_update(&self, msg: &AccountEvent) {
        let AccountData::Symbol(SymbolAccountData::OrderUpdate(update)) = &msg.data else {
            return;
        };
        let Some(client_order_id) = &update.client_order_id else {
            return;
        };
        self.gateway.ack(client_order_id);
    }
}

impl Task for OutcomeProcessor {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                Msg::Outcome(tagged) => self.on_outcome(tagged)?,
                Msg::OrderUpdate(event) => self.on_order_update(&event),
            }
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        tracing::info!("OutcomeProcessor stopped");
        Ok(())
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

}

/// 对 [`OrderGateway::place`] 的返回值契约直测（不是复刻实现），以及 [`OutcomeProcessor`]
/// 的总线装配。
///
/// 把 `ExchangeSpoke` 折算成"成功"曾是一个 Critical —— WS 拒单先到、REST 错误后到时，
/// 没挂上的单被当成成功。
#[cfg(test)]
mod gateway_tests {
    use super::*;
    use crate::domain::{
        AccountInfo, ExchangeError, OrderId, OrderType, Position, RejectReason, Side, TimeInForce,
    };
    use crate::domain::StepFormatter;
    use crate::engine::live::test_support::collect;
    use crate::tasks::Root;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";
    const CID: &str = "x-test-1";

    /// place_order 的可编程行为：测试注入结果；`gate` 存在时先通知已进入、再等放行
    /// （用于在 REST 在途期间模拟"WS 回报先到"）。
    struct FakeClient {
        result: Mutex<Option<Result<OrderId, PlaceError>>>,
        gate: Option<(mpsc::Sender<()>, tokio::sync::Mutex<mpsc::Receiver<()>>)>,
    }

    #[async_trait::async_trait]
    impl AccountClient for FakeClient {
        fn exchange(&self) -> Exchange {
            EX
        }
        async fn place_order(&self, _order: ExchangeOrder) -> Result<OrderId, PlaceError> {
            if let Some((entered_tx, proceed_rx)) = &self.gate {
                entered_tx.send(()).await.expect("通知已进入 place_order");
                proceed_rx.lock().await.recv().await.expect("等待放行");
            }
            self.result.lock().unwrap().take().expect("结果只取一次")
        }
        async fn cancel_order(&self, _s: &Symbol, _o: &OrderId) -> Result<(), ExchangeError> {
            Ok(())
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
                min_notional: 0.0,
                contract_size: 1.0,
            },
        )]))
    }

    /// 网关的停机开关接在测试树的根上：根进入停机即表示网关触发了停机
    struct Harness {
        root: Root,
        group: crate::tasks::Group,
        account_bus: AccountBus,
        gateway: Arc<OrderGateway>,
        /// 账户总线上的回报，供断言"是否发了 Error 回报"
        events: Arc<Mutex<Vec<AccountEvent>>>,
    }

    impl Harness {
        async fn new(client: FakeClient) -> Self {
            let root = Root::new("t");
            let group = root.group("g");
            let account_bus = AccountBus::new("account");
            let events = collect(&group, &account_bus).await;
            let clients: HashMap<Exchange, Arc<dyn AccountClient>> =
                HashMap::from([(EX, Arc::new(client) as Arc<dyn AccountClient>)]);
            let gateway = Arc::new(OrderGateway::new(
                clients,
                account_bus.clone(),
                metas(),
                false,
                root.fail_handle(),
                root.machine_stop_flag(),
            ));
            Self { root, group, account_bus, gateway, events }
        }

        /// 是否触发了停机（停机开关是同步生效的）
        async fn halted(&self) -> bool {
            self.root.is_stopping()
        }

        async fn finish(self) {
            // 触发过停机的用例这里会拿到那个错误，不关心
            let _ = self.root.shutdown().await;
        }

        /// 是否发出了 Error 终态回报（策略侧失败闭环）
        async fn error_reported(&self) -> bool {
            // 给总线转发留时间
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.events.lock().unwrap().iter().any(|e| {
                matches!(
                    &e.data,
                    AccountData::Symbol(SymbolAccountData::OrderUpdate(u))
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
        h.finish().await;
    }

    /// reduce-only 因仓位已平被拒 → 编排方得"目标已达成"，策略侧照常收 Error 闭环
    #[tokio::test]
    async fn reduce_only_closed_is_already_closed() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(PlaceError::Rejected(RejectReason::ReduceOnlyClosed)))),
            gate: None,
        })
        .await;
        let verdict = h.gateway.place(order(), "t").await;
        assert_eq!(verdict, Ok(PlaceVerdict::ReduceOnlyAlreadyClosed));
        assert!(h.error_reported().await, "策略侧仍需 Error 回报清 pending");
        h.finish().await;
    }

    /// 被拒绝（交易所从未回报）→ Err，且发 Error 回报让策略清挂单；不停机
    #[tokio::test]
    async fn rejection_is_err_with_feedback() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(PlaceError::rejected("Margin is insufficient.")))),
            gate: None,
        })
        .await;
        let verdict = h.gateway.place(order(), "t").await;
        assert!(verdict.is_err(), "拒单必须是 Err: {verdict:?}");
        assert!(h.error_reported().await, "拒单必须反馈 Error 回报");
        assert!(!h.halted().await, "拒单不该停机");
        h.finish().await;
    }

    /// 结果不确定且交易所没回报过 → 停机；**不**发 Error（否则策略以为单子没了、再挂一张）；
    /// 此后的新单一律不发（FakeClient 只备了一份结果，再调就 panic）
    #[tokio::test]
    async fn indeterminate_halts_without_feedback_and_stops_new_orders() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(PlaceError::indeterminate("HTTP 503 Unknown error")))),
            gate: None,
        })
        .await;
        let verdict = h.gateway.place(order(), "t").await;
        assert!(verdict.is_err(), "结果不确定不是成功: {verdict:?}");
        assert!(h.halted().await, "结果不确定必须停机");
        assert!(!h.error_reported().await, "结果不确定不能告诉策略单子没了");

        let again = h.gateway.place(order(), "t").await;
        assert!(again.is_err(), "停机期间不再发新单: {again:?}");
        h.finish().await;
    }

    /// 整机因**别的原因**开始停机（如持仓对账确认漂移）→ 新单不再发到交易所、不反馈 Error；
    /// 撤单照常放行（#710）。FakeClient 没备下单结果，误发就会 panic。
    #[tokio::test]
    async fn machine_stopping_refuses_new_orders_but_allows_cancel() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(None),
            gate: None,
        })
        .await;
        h.root.fail_handle().fail(anyhow::anyhow!("持仓漂移（模拟）"));

        let verdict = h.gateway.place(order(), "t").await;
        assert!(verdict.is_err(), "停机途中不该发新单: {verdict:?}");
        assert!(!h.error_reported().await, "停机拒发不反馈 Error（策略正在停）");

        let cancel = h
            .gateway
            .cancel(EX, SYM.to_string(), "ex-1".to_string(), CID.to_string())
            .await;
        assert_eq!(cancel, Ok(()), "停机途中撤单必须放行");
        h.finish().await;
    }

    /// **Critical 回归防线**：REST 在途期间交易所已回报（内容未知，可能是拒单）→
    /// 结论必须是 `ExchangeSpoke` 而不是"成功"，且不再发假的 Error 回报。
    #[tokio::test]
    async fn ws_report_during_rest_flight_is_exchange_spoke_not_success() {
        let (entered_tx, mut entered_rx) = mpsc::channel(1);
        let (proceed_tx, proceed_rx) = mpsc::channel(1);
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(PlaceError::indeterminate("late 503")))),
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
        assert!(!h.halted().await, "交易所回报流已认领该单，状态以回报为准，不必停机");
        h.finish().await;
    }

    // ===== OutcomeProcessor 的总线装配：订阅过滤、回报作废 REST 结论、停机等在途请求 =====

    fn signal(account: AccountId) -> AccountOutcome {
        AccountOutcome {
            account,
            event: OutcomeEvent::PlaceOrders {
                orders: vec![order()],
                comment: "t".to_string(),
            },
        }
    }

    async fn spawn_processor(h: &Harness) -> OutcomeBus {
        let outcome_bus = OutcomeBus::new("outcome");
        OutcomeProcessor::spawn(&h.group, h.gateway.clone(), &outcome_bus, &h.account_bus)
            .await
            .expect("OutcomeProcessor 起不来");
        outcome_bus
    }

    /// 实盘信号经总线到达网关；模拟账户的信号不归实盘出口
    /// （FakeClient 只备一份结果，误认领会让请求子任务 panic、根进入停机）
    #[tokio::test]
    async fn live_signal_reaches_gateway_and_paper_signal_is_ignored() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(PlaceError::rejected("Margin is insufficient.")))),
            gate: None,
        })
        .await;
        let outcome_bus = spawn_processor(&h).await;
        outcome_bus.publish(signal(AccountId::Live));
        assert!(h.error_reported().await, "实盘信号应经网关下单（此处被拒、反馈 Error）");
        outcome_bus.publish(signal(AccountId::Paper("BTC".to_string())));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!h.halted().await, "模拟账户的信号被实盘出口认领了");
        h.finish().await;
    }

    /// 账户总线上的交易所回报经本组件作废在途 REST 的结论：迟到的失败不再上报、不停机
    #[tokio::test]
    async fn exchange_report_on_bus_voids_late_rest_failure() {
        let (entered_tx, mut entered_rx) = mpsc::channel(1);
        let (proceed_tx, proceed_rx) = mpsc::channel(1);
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Err(PlaceError::indeterminate("late 503")))),
            gate: Some((entered_tx, tokio::sync::Mutex::new(proceed_rx))),
        })
        .await;
        let outcome_bus = spawn_processor(&h).await;
        outcome_bus.publish(signal(AccountId::Live));
        entered_rx.recv().await.expect("place_order 已进入");
        h.account_bus.publish(AccountEvent {
            account: AccountId::Live,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Symbol(SymbolAccountData::OrderUpdate(OrderUpdate {
                order_id: "ex-1".to_string(),
                client_order_id: Some(CID.to_string()),
                exchange: EX,
                symbol: SYM.to_string(),
                side: Side::Short,
                status: OrderStatus::Pending,
                quantity: 1.0,
            })),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        proceed_tx.send(()).await.expect("放行 REST 返回失败");
        assert!(!h.error_reported().await, "交易所已回报过，迟到的 REST 失败不该再发假 Error");
        assert!(!h.halted().await, "交易所回报流已认领该单，不必停机");
        h.finish().await;
    }

    /// 停机等在途请求返回才算停完（请求挂在本组件的子树上，不是游离任务）
    #[tokio::test]
    async fn shutdown_waits_for_in_flight_request() {
        let (entered_tx, mut entered_rx) = mpsc::channel(1);
        let (proceed_tx, proceed_rx) = mpsc::channel(1);
        let h = Harness::new(FakeClient {
            result: Mutex::new(Some(Ok("ex-1".to_string()))),
            gate: Some((entered_tx, tokio::sync::Mutex::new(proceed_rx))),
        })
        .await;
        let outcome_bus = spawn_processor(&h).await;
        outcome_bus.publish(signal(AccountId::Live));
        entered_rx.recv().await.expect("place_order 已进入");
        let shutdown = h.root.shutdown();
        tokio::pin!(shutdown);
        let early = tokio::time::timeout(std::time::Duration::from_millis(100), &mut shutdown).await;
        assert!(early.is_err(), "在途请求还没返回，停机不该完成");
        proceed_tx.send(()).await.expect("放行 REST");
        shutdown.await.expect("正常停机");
    }
}

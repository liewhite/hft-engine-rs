//! 实盘出向：OrderGateway（唯一下单出口）+ OutcomeProcessor（总线订阅薄壳）。
//!
//! **系统的出向单位边界**：策略与 `StrategyRunner` 产出的订单一律币本位，在此转为
//! [`ExchangeOrder`]（换成交易所单位 + 数量下界校验；精度取整已在 `StrategyRunner` 做过）后才发给交易所。回测不经过本模块，
//! 因此 `SimState` 拿到的同样是币本位订单 —— 两条路径在策略侧口径一致。

use crate::domain::{
    now_ms, AccountId, Exchange, Order, PlaceError, RejectReason, Symbol, SymbolMeta,
};
use crate::exchange::{AccountClient, ExchangeOrder};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, OrderRequestResult, RequestOutcome, SymbolAccountData,
    PLACE_TIMEOUT_MS,
};
use crate::strategy::OutcomeEvent;
use crate::tasks::{Group, Inbox, Scope, StopFlag, Task};
use std::collections::HashMap;
use std::sync::Arc;

use super::{to_live_outlet, AccountOutcome, OutcomeBus};

// ============================================================================
// OrderGateway —— 唯一的实盘下单出口
// ============================================================================

/// 实盘网关发往各所的下单 / 撤单 REST 请求累计次数（本进程启动以来）。网关是唯一写入方，
/// 指标出口只读（推成 `hft_order_requests`，看各所下单、撤单用了多少次）。口径：交给交易所接口的
/// 次数，含被本地限频拒掉、没发出去的；出向校验不过（数量不足一手等）没交给接口的不算。
#[derive(Debug, Default)]
pub struct OrderRequestCounts(std::sync::Mutex<HashMap<(Exchange, RequestKind), u64>>);

/// 计数的请求种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    Place,
    Cancel,
}

impl RequestKind {
    pub fn label(self) -> &'static str {
        match self {
            RequestKind::Place => "place",
            RequestKind::Cancel => "cancel",
        }
    }
}

impl OrderRequestCounts {
    fn bump(&self, exchange: Exchange, kind: RequestKind) {
        *self
            .0
            .lock()
            .expect("请求计数锁被毒化")
            .entry((exchange, kind))
            .or_default() += 1;
    }

    #[cfg(test)]
    pub(crate) fn bump_for_test(&self, exchange: Exchange, kind: RequestKind, n: u64) {
        for _ in 0..n {
            self.bump(exchange, kind);
        }
    }

    /// 当前各 (所, 种类) 的累计次数
    pub fn snapshot(&self) -> Vec<(Exchange, RequestKind, u64)> {
        let mut v: Vec<_> = self
            .0
            .lock()
            .expect("请求计数锁被毒化")
            .iter()
            .map(|(&(ex, k), &n)| (ex, k, n))
            .collect();
        v.sort_by_key(|&(ex, k, _)| (ex.to_string(), k.label()));
        v
    }
}

/// 实盘出向执行器：系统里**唯一**知道"怎么把订单发到交易所"的组件。
///
/// 由 [`OutcomeProcessor`] 独占：策略信号各起一个子任务执行，不阻塞事件流。
///
/// 每次请求的结论以 [`OrderRequestResult`] 发到账户总线（策略的订单列表据此推进），
/// 只有一种例外：下单结果不确定时不发（见 [`Self::place`]）。
pub struct OrderGateway {
    /// 各所的账户私有能力。键集 = 配了凭证的所；实盘实例只会对这些所下单（投产时校验）
    accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 账户总线（发布请求结论，标 Live）
    account_bus: AccountBus,
    /// Symbol 元数据：用于把币本位订单折算为交易所下单单位
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 整机是否已开始停机（任何原因：持仓漂移、外部停机信号……）。
    /// 一旦停机就不再发新单：停机收尾期间策略可能还在出信号，这些单挂到交易所上、进程退出后
    /// 无人看管（#710）。撤单不受影响。
    machine_stopping: StopFlag,
    /// 下单 / 撤单请求计数（口径见 [`OrderRequestCounts`]）
    requests: Arc<OrderRequestCounts>,
}

impl OrderGateway {
    /// `machine_stopping`：manager 的 `scope.machine_stop_flag()`
    pub fn new(
        accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
        account_bus: AccountBus,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        machine_stopping: StopFlag,
    ) -> Self {
        Self {
            accounts,
            account_bus,
            symbol_metas,
            machine_stopping,
            requests: Arc::default(),
        }
    }

    /// 请求计数（交给指标出口读）
    pub fn request_counts(&self) -> Arc<OrderRequestCounts> {
        self.requests.clone()
    }

    /// 下一张单：出向校验（换单位 + 下界）→ REST → 按三种结果处置。
    ///
    /// # 三种结果（见 [`PlaceError`]）
    ///
    /// - 成功：回报 [`RequestOutcome::Placed`]（带交易所订单号），此后交易所回报接手。
    /// - 被拒绝（单子确定没挂上）：回报 [`RequestOutcome::PlaceRejected`]。交易所已对这张单说过话的，
    ///   订单列表以交易所为准、不理这条（见 `messaging::SymbolOrders`）。
    /// - 结果说不清（超时、5xx，单子可能已挂上或已成交）：打 error、**不**回报，单留在「下单中」等交易所回报；
    ///   [`PLACE_TIMEOUT_MS`] 内都没有就由订单列表按下单失败处理（#899 用户定，不停机）。
    ///
    /// 整机一旦开始停机（不论什么原因），新单一律不发，也不回报（策略正在停，没人再按它重挂）。
    pub async fn place(&self, order: Order, comment: &str) {
        if self.machine_stopping.is_stopping() {
            tracing::warn!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                client_order_id = %order.client_order_id,
                "引擎正在停机，不再发新单"
            );
            return;
        }

        // 下单范围闸门（`StrategyRunner::admit`）已保证：所已装配、有合约规格；实盘实例要交易的所
        // 投产时已校验配了凭证（`PlannedAccount::bind`）。这里查不到就是程序写错，停机
        let client = self
            .accounts
            .get(&order.exchange)
            .cloned()
            .unwrap_or_else(|| panic!("{} 没有账户却收到实盘下单（投产校验漏了）", order.exchange));
        let meta = self
            .symbol_metas
            .get(&(order.exchange, order.symbol.clone()))
            .unwrap_or_else(|| {
                panic!(
                    "{}/{} 没有合约规格却收到下单（下单闸门漏了）",
                    order.exchange, order.symbol
                )
            });

        tracing::info!(
            exchange = %order.exchange,
            symbol = %order.symbol,
            side = %order.side,
            order_type = ?order.order_type,
            quantity = %order.quantity,
            client_order_id = %order.client_order_id,
            signal = %comment,
            "Placing order"
        );

        // 币本位 -> 交易所单位，并校验数量下界（取整为 0 / 低于最小下单量）：这种单发出去必被
        // 交易所拒，本地拦下让失败原因可定位，且不白打 REST
        let exchange_order = match ExchangeOrder::from_domain(order.clone(), meta) {
            Ok(wire) => wire,
            Err(reason) => {
                tracing::error!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    client_order_id = %order.client_order_id,
                    %reason,
                    "订单未通过出向校验，未发送"
                );
                self.report(
                    &order.exchange,
                    &order.symbol,
                    &order.client_order_id,
                    RequestOutcome::PlaceRejected { reason },
                );
                return;
            }
        };

        self.requests.bump(order.exchange, RequestKind::Place);
        match client.place_order(exchange_order).await {
            Ok(order_id) => {
                tracing::info!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    %order_id,
                    client_order_id = %order.client_order_id,
                    "Order placed successfully"
                );
                self.report(
                    &order.exchange,
                    &order.symbol,
                    &order.client_order_id,
                    RequestOutcome::Placed { order_id },
                );
            }
            Err(PlaceError::Rejected(reason)) => self.rejected(&order, reason),
            Err(PlaceError::Indeterminate(detail)) => tracing::error!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                side = %order.side,
                order_type = ?order.order_type,
                quantity = %order.quantity,
                client_order_id = %order.client_order_id,
                %detail,
                timeout_ms = PLACE_TIMEOUT_MS,
                "下单结果说不清（单子可能已挂上或已成交）：留在下单中等交易所回报，超时按下单失败处理"
            ),
        }
    }

    /// 被拒绝：单子确定没挂上，回报给订单列表。reduce-only 因仓位已平被拒是无害的，只记 info。
    fn rejected(&self, order: &Order, reason: RejectReason) {
        if reason == RejectReason::ReduceOnlyClosed {
            tracing::info!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                client_order_id = %order.client_order_id,
                "Reduce-only order rejected: position already closed"
            );
        } else {
            tracing::error!(
                exchange = %order.exchange,
                symbol = %order.symbol,
                client_order_id = %order.client_order_id,
                %reason,
                "Order rejected"
            );
        }
        let reason = PlaceError::Rejected(reason).to_string();
        self.report(
            &order.exchange,
            &order.symbol,
            &order.client_order_id,
            RequestOutcome::PlaceRejected { reason },
        );
    }

    /// 按客户端订单号撤一张单。成功不回报（交易所随后推 `Cancelled`，或撤单生效前已成交）；
    /// 失败回报 [`RequestOutcome::CancelFailed`]，单是否还挂着以交易所随后的订单回报为准，
    /// 要不要重撤由策略定。整机停机途中照常撤单。
    pub async fn cancel(&self, exchange: Exchange, symbol: Symbol, client_order_id: String) {
        // 撤的是跟踪表里的单，它下单时已过账户检查（见 [`Self::place`]）
        let client = self
            .accounts
            .get(&exchange)
            .unwrap_or_else(|| panic!("{exchange} 没有账户却收到实盘撤单（投产校验漏了）"));
        tracing::info!(%exchange, %symbol, %client_order_id, "Cancelling order");
        self.requests.bump(exchange, RequestKind::Cancel);
        let result = client
            .cancel_order(&symbol, &client_order_id)
            .await
            .map_err(|e| e.to_string());
        match result {
            Ok(()) => {
                tracing::info!(%exchange, %symbol, %client_order_id, "Cancel request accepted")
            }
            Err(reason) => {
                // 最常见的原因是单刚成交或刚撤掉（交易所说「订单不存在」），终态回报随后就到
                tracing::warn!(%exchange, %symbol, %client_order_id, %reason, "撤单失败");
                self.report(
                    &exchange,
                    &symbol,
                    &client_order_id,
                    RequestOutcome::CancelFailed { reason },
                );
            }
        }
    }

    /// 发布一次请求的结论（策略的订单列表据此推进）
    fn report(
        &self,
        exchange: &Exchange,
        symbol: &Symbol,
        client_order_id: &str,
        outcome: RequestOutcome,
    ) {
        let local_ts = now_ms();
        self.account_bus.publish(AccountEvent {
            account: AccountId::Live,
            exchange_ts: local_ts, // 本地结论，没有交易所时间戳
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::OrderRequest(OrderRequestResult {
                exchange: *exchange,
                symbol: symbol.clone(),
                client_order_id: client_order_id.to_string(),
                outcome,
            })),
        });
    }
}

// ============================================================================
// OutcomeProcessor —— 总线订阅薄壳
// ============================================================================

/// 订阅出向总线，把实盘账户的策略信号交给 [`OrderGateway`]。
pub struct OutcomeProcessor {
    gateway: Arc<OrderGateway>,
    inbox: Inbox<AccountOutcome>,
    /// 在途的下单/撤单请求，每张单一个子任务（见 [`Self::on_outcome`]）
    requests: Group,
}

impl OutcomeProcessor {
    /// 在 `group` 下起实盘出口，等它订阅完成。
    ///
    /// 订阅 `outcome_bus`，过滤 [`to_live_outlet`]：只认领实盘账户的信号（与本地柜台的
    /// `to_paper_outlet` 成对，判据只有 `outlet_for` 一处）。
    pub async fn spawn(
        group: &Group,
        gateway: Arc<OrderGateway>,
        outcome_bus: &OutcomeBus,
    ) -> anyhow::Result<()> {
        let outcome_bus = outcome_bus.clone();
        group
            .spawn("OutcomeProcessor", move |scope: Scope| async move {
                let (_addr, mut inbox) = scope.inbox::<AccountOutcome>();
                inbox.subscribe(&outcome_bus, to_live_outlet, |o| o);
                let requests = scope.group("requests");
                tracing::info!("OutcomeProcessor started");
                Ok(Self {
                    gateway,
                    inbox,
                    requests,
                })
            })
            .await?;
        Ok(())
    }

    fn on_outcome(&self, tagged: AccountOutcome) -> anyhow::Result<()> {
        // 归属由订阅过滤器保证（见 [`Self::spawn`]，判据只有 `outlet_for` 一处）
        // 下单/撤单一律各起一个子任务异步执行，**有意为之**：REST 往返可能上百 ms，
        // 若在收件循环里 await，后面的信号全部排队等它。
        //
        // 子任务挂在本组件的 `requests` 组下而不是游离的 `tokio::spawn`：
        // - 停机时本组件先排空收件箱（排空期间起的单也挂进来），框架再等全部在途请求返回
        //   才算停完 —— 不再有"引擎已停但请求仍在途"的窗口（REST 有自己的超时，根的总期限兜底）；
        // - 请求任务 panic 会作为错误上冒、整机停机，不会被吞掉。
        //
        // 框架不保证到达序：同一 symbol 的多笔下单/撤单并发发出，可能乱序到达交易所。
        // 需要先后的策略自己等前一张的结论（请求结论或订单回报）再发下一张。
        //
        // 子任务内的结论由 gateway 以 OrderRequestResult 发到账户总线（见 `OrderGateway::place`）。
        match tagged.event {
            OutcomeEvent::CancelOrder {
                exchange,
                symbol,
                client_order_id,
            } => {
                let gateway = self.gateway.clone();
                self.submit(format!("cancel:{client_order_id}"), async move {
                    gateway.cancel(exchange, symbol, client_order_id).await;
                })?;
            }
            OutcomeEvent::PlaceOrders { orders, comment } => {
                // 同一信号里的几张单各自独立下单：不保证同时成交或同时失败，成交不齐由策略处理
                for order in orders {
                    let gateway = self.gateway.clone();
                    let comment = comment.clone();
                    let name = format!("place:{}", order.client_order_id);
                    self.submit(name, async move {
                        gateway.place(order, &comment).await;
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
}

impl Task for OutcomeProcessor {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(tagged) = self.inbox.recv().await {
            self.on_outcome(tagged)?;
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        tracing::info!("OutcomeProcessor stopped");
        Ok(())
    }
}

/// [`OrderGateway`] 的三种下单结果与撤单结论（看它发到账户总线的回报和是否停机），以及
/// [`OutcomeProcessor`] 的总线装配。
#[cfg(test)]
mod gateway_tests {
    use super::*;
    use crate::domain::PriceStep;
    use crate::domain::{ExchangeError, OrderId, OrderType, Position, Side, TimeInForce};
    use crate::engine::live::test_support::collect;
    use crate::tasks::Root;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";
    const CID: &str = "x-test-1";

    /// place_order / cancel_order 的可编程行为：测试注入结果；`gate` 存在时先通知已进入、再等放行
    /// （用于在 REST 在途期间模拟"WS 回报先到"）。
    struct FakeClient {
        result: Mutex<Option<Result<OrderId, PlaceError>>>,
        gate: Option<(mpsc::Sender<()>, tokio::sync::Mutex<mpsc::Receiver<()>>)>,
        cancel_result: Result<(), String>,
    }

    impl FakeClient {
        fn placing(result: Result<OrderId, PlaceError>) -> Self {
            Self {
                result: Mutex::new(Some(result)),
                gate: None,
                cancel_result: Ok(()),
            }
        }
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
        async fn cancel_order(&self, _s: &Symbol, _cid: &str) -> Result<(), ExchangeError> {
            self.cancel_result.clone().map_err(ExchangeError::new)
        }
        async fn fetch_open_orders(
            &self,
        ) -> Result<Vec<crate::exchange::OpenOrder>, ExchangeError> {
            unreachable!()
        }
        async fn cancel_open_order(
            &self,
            _: &crate::exchange::OpenOrder,
        ) -> Result<(), ExchangeError> {
            unreachable!()
        }
        async fn fetch_equity(&self) -> Result<crate::domain::Money, ExchangeError> {
            unreachable!()
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            unreachable!()
        }
    }

    fn order() -> Order {
        Order {
            exchange: EX,
            symbol: SYM.to_string(),
            side: Side::Short,
            order_type: OrderType::Limit {
                price: dec!(100.0),
                tif: TimeInForce::GTC,
            },
            quantity: dec!(1.0),
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
                price_rule: Arc::new(PriceStep::new(
                    crate::domain::PositiveDecimal::new(dec!(0.1)).unwrap(),
                )),
                size_step: crate::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
                min_order_size: crate::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
                min_notional: 0.0,
                contract_size: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
                quote: "USDT".to_string(),
            },
        )]))
    }

    /// 网关的停机开关接在测试树的根上：根进入停机即表示网关触发了停机
    struct Harness {
        root: Root,
        group: crate::tasks::Group,
        gateway: Arc<OrderGateway>,
        /// 账户总线上的事件，供断言网关发了什么回报
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
                root.machine_stop_flag(),
            ));
            Self {
                root,
                group,
                gateway,
                events,
            }
        }

        /// 是否触发了停机（停机开关是同步生效的）
        async fn halted(&self) -> bool {
            self.root.is_stopping()
        }

        async fn finish(self) {
            // 触发过停机的用例这里会拿到那个错误，不关心
            let _ = self.root.shutdown().await;
        }

        /// 网关发到账户总线的请求结论
        async fn reports(&self) -> Vec<RequestOutcome> {
            // 给总线转发留时间
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match &e.data {
                    AccountData::Symbol(SymbolAccountData::OrderRequest(r)) => {
                        assert_eq!(
                            (r.exchange, r.symbol.as_str(), r.client_order_id.as_str()),
                            (EX, SYM, CID)
                        );
                        Some(r.outcome.clone())
                    }
                    _ => None,
                })
                .collect()
        }
    }

    fn is_rejected(r: &[RequestOutcome]) -> bool {
        matches!(r, [RequestOutcome::PlaceRejected { .. }])
    }

    /// REST 受理 → 回报 Placed（带交易所订单号）
    #[tokio::test]
    async fn rest_accept_reports_placed() {
        let h = Harness::new(FakeClient::placing(Ok("ex-1".to_string()))).await;
        h.gateway.place(order(), "t").await;
        assert_eq!(
            h.reports().await,
            vec![RequestOutcome::Placed {
                order_id: "ex-1".into()
            }]
        );
        h.finish().await;
    }

    /// 被拒绝（交易所从未回报）→ 回报 PlaceRejected 让跟踪表移出；不停机。reduce-only 已平同样
    #[tokio::test]
    async fn rejection_is_reported_without_halting() {
        for reason in [
            RejectReason::Other("Margin is insufficient.".into()),
            RejectReason::ReduceOnlyClosed,
        ] {
            let h = Harness::new(FakeClient::placing(Err(PlaceError::Rejected(reason)))).await;
            h.gateway.place(order(), "t").await;
            assert!(is_rejected(&h.reports().await), "拒单必须回报");
            assert!(!h.halted().await, "拒单不该停机");
            h.finish().await;
        }
    }

    /// 出向校验不过（数量取整为 0）→ 不发 REST，回报 PlaceRejected
    #[tokio::test]
    async fn local_validation_failure_is_reported_as_rejected() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(None),
            gate: None,
            cancel_result: Ok(()),
        })
        .await;
        h.gateway
            .place(
                Order {
                    quantity: dec!(0.0001),
                    ..order()
                },
                "t",
            )
            .await;
        assert!(is_rejected(&h.reports().await));
        h.finish().await;
    }

    /// 结果说不清 → 不回报（否则策略以为单子没了、再挂一张），也不停机：单留在下单中，由订单列表等交易所
    /// 回报、超时按失败处理（#899）
    #[tokio::test]
    async fn indeterminate_neither_reports_nor_halts() {
        let h = Harness::new(FakeClient::placing(Err(PlaceError::indeterminate(
            "HTTP 503 Unknown error",
        ))))
        .await;
        h.gateway.place(order(), "t").await;
        assert!(!h.halted().await, "结果说不清不停机");
        assert!(
            h.reports().await.is_empty(),
            "结果说不清不能告诉策略单子没了"
        );
        h.finish().await;
    }

    /// 整机因**别的原因**开始停机（如持仓对账确认漂移）→ 新单不再发到交易所、不回报；
    /// 撤单照常放行（#710）。FakeClient 没备下单结果，误发就会 panic。
    #[tokio::test]
    async fn machine_stopping_refuses_new_orders_but_allows_cancel() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(None),
            gate: None,
            cancel_result: Err("Unknown order sent.".into()),
        })
        .await;
        h.root
            .fail_handle()
            .fail(anyhow::anyhow!("持仓漂移（模拟）"));

        h.gateway.place(order(), "t").await;
        assert!(h.reports().await.is_empty(), "停机拒发不回报（策略正在停）");

        h.gateway.cancel(EX, SYM.to_string(), CID.to_string()).await;
        assert!(
            matches!(
                h.reports().await.as_slice(),
                [RequestOutcome::CancelFailed { .. }]
            ),
            "停机途中撤单照常发出（此处撤单失败，如实回报）"
        );
        h.finish().await;
    }

    /// 撤单成功不回报（交易所随后推 Cancelled）；失败回报 CancelFailed
    #[tokio::test]
    async fn cancel_reports_only_failures() {
        let h = Harness::new(FakeClient {
            result: Mutex::new(None),
            gate: None,
            cancel_result: Ok(()),
        })
        .await;
        h.gateway.cancel(EX, SYM.to_string(), CID.to_string()).await;
        assert!(h.reports().await.is_empty());
        h.finish().await;
    }

    // ===== OutcomeProcessor 的总线装配：订阅过滤、停机等在途请求 =====

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
        OutcomeProcessor::spawn(&h.group, h.gateway.clone(), &outcome_bus)
            .await
            .expect("OutcomeProcessor 起不来");
        outcome_bus
    }

    /// 实盘信号经总线到达网关；模拟账户的信号不归实盘出口
    /// （FakeClient 只备一份结果，误认领会让请求子任务 panic、根进入停机）
    #[tokio::test]
    async fn live_signal_reaches_gateway_and_paper_signal_is_ignored() {
        let h = Harness::new(FakeClient::placing(Err(PlaceError::rejected(
            "Margin is insufficient.",
        ))))
        .await;
        let outcome_bus = spawn_processor(&h).await;
        outcome_bus.publish(signal(AccountId::Live));
        assert!(
            is_rejected(&h.reports().await),
            "实盘信号应经网关下单（此处被拒、回报）"
        );
        outcome_bus.publish(signal(AccountId::Paper("BTC".to_string())));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!h.halted().await, "模拟账户的信号被实盘出口认领了");
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
            cancel_result: Ok(()),
        })
        .await;
        let outcome_bus = spawn_processor(&h).await;
        outcome_bus.publish(signal(AccountId::Live));
        entered_rx.recv().await.expect("place_order 已进入");
        let shutdown = h.root.shutdown();
        tokio::pin!(shutdown);
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut shutdown).await;
        assert!(early.is_err(), "在途请求还没返回，停机不该完成");
        proceed_tx.send(()).await.expect("放行 REST");
        shutdown.await.expect("正常停机");
    }
}

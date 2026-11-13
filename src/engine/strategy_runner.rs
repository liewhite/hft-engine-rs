//! StrategyRunner —— 策略执行的**纯逻辑核心**。
//!
//! 把一个事件喂给策略、维护策略独享状态、产出策略信号。不含任何
//! 传输/并发设施 (无 actor / channel / pubsub)，因此被两种驱动复用：
//!   - 实盘 [`crate::engine::Executor`]：按收件箱串行消费总线事件，调用 [`StrategyRunner::on_event`]
//!     后把结果发布到出向总线
//!   - 回测 [`crate::backtest::BacktestEngine`]：单线程虚拟时间循环里同步调用 [`StrategyRunner::accepts`] /
//!     [`StrategyRunner::on_event`]
//!
//! 职责：过滤订阅范围、更新 StateManager、把策略的下单意图变成订单（**下单范围的唯一闸门**：
//! (所, 币) 必须在策略的订阅范围内、有合约规格、该所已装配，否则停机；再分配 client_order_id、
//! 按交易所精度取整价格与数量）、登记订单跟踪表（登记的就是发出的那张单）。
//!
//! **只取整，不换单位**：产出的订单一律**币本位** (见 [`crate::domain::Quantity`])，但价格与
//! 数量已按交易所精度取整。两件事必须分在两处：
//!   - 精度取整是**市场规则**，回测/模拟盘也必须遵守，否则 `SimState` 会用非法 tick 价与
//!     非法步长撮合，结果失真 —— 故留在此处，两条驱动共享
//!   - 单位折算 (币 -> 张) 是**线路细节**，只发生在实盘出口，见
//!     [`crate::exchange::ExchangeOrder`]；回测不经过那里，因此全程币本位

use crate::domain::{AccountId, Exchange, Order, OrderIntent, OrderType, Symbol, SymbolMeta};
use crate::messaging::{IncomeEvent, StateManager, SubscriptionScope};
use crate::strategy::{OutcomeEvent, Signal, Strategy, StrategyView};
use std::collections::HashMap;
use std::sync::Arc;

/// client_order_id 生成策略 (注入点)。
///
/// 生产用各所适配层的格式生成随机 id（[`AdapterClientOrderIds`]），保证跨重启唯一；回测用确定性
/// 自增计数，保证"同一输入 -> 同一逐笔回报序列" (client_order_id 会随订单/成交回报投递给策略)。
pub trait ClientOrderIdGen: Send {
    /// `None` = 该所没有装配（策略对没接入的所下单），驱动方停机
    fn next_id(&mut self, exchange: Exchange) -> Option<String>;
}

/// 某所生成本引擎订单号的函数：格式是各所的线路知识，由适配层随装配产物提供
/// （见 [`crate::exchange::CliOrderIdFormat`]），框架只调用、不解读
pub type NewClientOrderId = fn() -> String;

/// 生产：按所调用适配层提供的订单号生成函数
pub struct AdapterClientOrderIds(pub Arc<HashMap<Exchange, NewClientOrderId>>);
impl ClientOrderIdGen for AdapterClientOrderIds {
    fn next_id(&mut self, exchange: Exchange) -> Option<String> {
        self.0.get(&exchange).map(|generate| generate())
    }
}

/// 回测确定性：自增计数 (与墙钟/随机无关)。
#[derive(Default)]
pub struct SequentialClientOrderIdGen {
    next: u64,
}
impl ClientOrderIdGen for SequentialClientOrderIdGen {
    fn next_id(&mut self, _exchange: Exchange) -> Option<String> {
        let id = format!("bt{}", self.next);
        self.next += 1;
        Some(id)
    }
}

pub struct StrategyRunner {
    strategy: Box<dyn Strategy>,
    state: StateManager,
    /// 用于按交易所精度取整（**不**用于单位折算）
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 策略订阅范围（判据实现见 [`SubscriptionScope::accepts`]，与实盘分发层同一份）
    subscriptions: SubscriptionScope,
    /// client_order_id 生成策略 (生产 UUID / 回测确定性计数)
    id_gen: Box<dyn ClientOrderIdGen>,
}

impl StrategyRunner {
    /// `account` 是本策略绑定的账户（只收它的私有事件）；`id_gen` 给策略的单分配订单号
    /// （实盘 [`AdapterClientOrderIds`]，回测 [`SequentialClientOrderIdGen`] 以确保确定性）。
    pub fn new(
        strategy: Box<dyn Strategy>,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        account: AccountId,
        id_gen: Box<dyn ClientOrderIdGen>,
    ) -> Self {
        // 从 public_streams 推导订阅范围
        let subscriptions = SubscriptionScope::new(
            account,
            strategy
                .public_streams()
                .iter()
                .flat_map(|(ex, kinds)| kinds.iter().map(move |k| (*ex, k.symbol().clone()))),
        );
        let symbols: Vec<Symbol> = subscriptions.symbols().cloned().collect();
        let state = StateManager::new(&symbols);
        Self {
            strategy,
            state,
            symbol_metas,
            subscriptions,
            id_gen,
        }
    }

    /// 只读访问策略状态 (供观察/调试)。
    pub fn state(&self) -> &StateManager {
        &self.state
    }

    /// 策略自己的观测读数（见 [`Strategy::gauges`]）
    pub fn strategy_gauges(
        &self,
        now: crate::domain::Timestamp,
    ) -> Vec<crate::observability::Gauge> {
        self.strategy.gauges(now)
    }

    /// 写入持仓基线（投产握手）。
    ///
    /// **实盘与模拟账户都会调用**：v0.16 起投产为作用域内每条腿都写基线，模拟账户写的是
    /// 全 0（从零起步的真值），为的是让"持仓未查询到"这个状态对策略不可达
    /// （见 `engine::live::AccountBinding::into_baselines`）。回测不经投产，不调用。
    ///
    /// 语义见 [`crate::messaging::PositionBaseline`] 与 `SymbolState::seed_position`。
    pub fn seed_positions(&mut self, baselines: &[crate::messaging::PositionBaseline]) {
        self.state.seed_positions(baselines);
    }

    /// 本策略绑定的账户
    pub fn account(&self) -> &AccountId {
        self.subscriptions.account()
    }

    /// 这条事件在本策略的订阅范围内吗（判据与实盘分发层共用，见 [`SubscriptionScope`]）
    pub fn accepts(&self, event: &IncomeEvent) -> bool {
        self.subscriptions.accepts(event)
    }

    /// 更新状态并运行策略，把策略的信号处理成发往出口的事件（下单意图已过范围闸门、分配订单号、按交易所
    /// 精度取整并登记跟踪表；撤单只放行下单已受理、还开着、没有撤单在途的单，所取自跟踪表，并记下「撤单已发」）。
    ///
    /// 产出订单是**币本位**且已取整；单位折算是出口的职责，见模块文档。
    ///
    /// `Err` = 订单跟踪表已说不清挂单与持仓（有单等交易所太久，见 [`crate::messaging::ORDER_STUCK_MS`]），
    /// 或策略对范围外的 (所, 币) 下单（见 [`Self::admit`]）、撤范围外的币的单，驱动方应停机。同一批里闸门前已登记的单
    /// 随停机作废：停机后不发任何新单（用户定 #710）。
    pub fn on_event(&mut self, event: &IncomeEvent) -> Result<Vec<OutcomeEvent>, String> {
        self.state.apply(event)?;
        let now = event.local_ts();
        // 单一分发入口：一切事件（含 BorrowFee/ExchangeRate）都走 on_event，
        // 策略自行 match 关心的变体 —— 不存在第二套 typed 回调。
        let raw = self
            .strategy
            .on_event(event, StrategyView::new(&self.state));
        let mut out = Vec::with_capacity(raw.len());
        for signal in raw {
            match signal {
                Signal::PlaceOrders { orders, comment } => {
                    let mut admitted = Vec::with_capacity(orders.len());
                    for intent in orders {
                        let order = self.admit(intent)?;
                        // created_at 取当前事件时刻 (回测=虚拟时间, 确定性), 不读墙钟
                        self.state.add_pending_order(order.clone(), now);
                        admitted.push(order);
                    }
                    out.push(OutcomeEvent::PlaceOrders {
                        orders: admitted,
                        comment,
                    });
                }
                Signal::CancelOrder {
                    symbol,
                    client_order_id,
                } => {
                    // 范围外的币没有跟踪表：与下单同一道闸门，策略写错了就停机
                    if self.state.symbol_state(&symbol).is_none() {
                        return Err(format!(
                            "策略撤订阅范围外的 {symbol} 的单（{client_order_id}）"
                        ));
                    }
                    // 所取自跟踪表里那张单，策略不填
                    match self.state.mark_cancel_sent(&symbol, &client_order_id, now) {
                        Ok(exchange) => out.push(OutcomeEvent::CancelOrder {
                            exchange,
                            symbol,
                            client_order_id,
                        }),
                        Err(why) => {
                            tracing::debug!(%symbol, %client_order_id, why, "撤单不发出")
                        }
                    }
                }
                // 自定义事件原样放行：无订单语义，不登记 pending、不过精度取整
                Signal::Emit(custom) => out.push(OutcomeEvent::Emit(custom)),
            }
        }
        Ok(out)
    }

    /// 下单范围闸门 + 赋号 + 取整：把一张下单意图变成要发出的订单。
    ///
    /// 三种 `Err`（都是策略写错或装配错，发出去就是不在跟踪表、不在对账名单里的真单，驱动方停机）：
    /// (所, 币) 不在策略的订阅范围内、没有合约规格（取不了整）、该所没有装配（无从赋号）。
    ///
    /// 取整只取整、不换单位，**保持币本位**：数量向下取到步长（不超出策略意图），价格取最近的合法 tick。
    /// 取整后为 0 或低于最小下单量的单照发，由出口（实盘 `ExchangeOrder` / 模拟柜台）拒掉并回报策略。
    fn admit(&mut self, intent: OrderIntent) -> Result<Order, String> {
        let OrderIntent {
            exchange,
            symbol,
            side,
            order_type,
            quantity,
            reduce_only,
        } = intent;
        if !self.subscriptions.contains(exchange, &symbol) {
            return Err(format!(
                "策略对订阅范围外的 {exchange}/{symbol} 下单（范围由 public_streams 声明）"
            ));
        }
        let meta = self
            .symbol_metas
            .get(&(exchange, symbol.clone()))
            .ok_or_else(|| format!("{exchange}/{symbol} 没有合约规格，无法按交易所精度取整"))?;
        let client_order_id = self
            .id_gen
            .next_id(exchange)
            .ok_or_else(|| format!("策略对没有装配的交易所 {exchange} 下单（{symbol}）"))?;
        let order_type = match order_type {
            OrderType::Market => OrderType::Market,
            OrderType::Limit { price, tif } => OrderType::Limit {
                price: meta.round_price(price),
                tif,
            },
        };
        Ok(Order {
            client_order_id,
            exchange,
            quantity: meta.round_coin_size_down(quantity),
            symbol,
            side,
            order_type,
            reduce_only,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PriceStep;
    use crate::domain::{OrderType, Side, TimeInForce};
    use crate::exchange::SubscriptionKind;
    use crate::messaging::{IncomeEvent, MarketData};
    use crate::strategy::{OutcomeEvent, Signal, Strategy};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::collections::HashSet;

    const EX: Exchange = Exchange::OKX;
    const SYM: &str = "BTC";
    /// OKX BTC-USDT-SWAP：每张 0.01 币，数量步长 1 张 -> 合法币量是 0.01 的整数倍
    const CONTRACT_SIZE: Decimal = dec!(0.01);

    fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        Arc::new(HashMap::from([(
            (EX, SYM.to_string()),
            SymbolMeta {
                exchange: EX,
                symbol: SYM.to_string(),
                price_rule: Arc::new(PriceStep::new(dec!(0.1))),
                size_step: dec!(1.0),
                min_order_size: dec!(1.0),
                min_notional: 0.0,
                contract_size: CONTRACT_SIZE,
            },
        )]))
    }

    /// 一次性下一张指定 (所, 币)、数量、价格的挂单
    struct OneShot {
        exchange: Exchange,
        symbol: &'static str,
        quantity: Decimal,
        price: Decimal,
        placed: bool,
    }

    impl OneShot {
        fn new(quantity: Decimal, price: Decimal) -> Self {
            Self::at(EX, SYM, quantity, price)
        }
        fn at(exchange: Exchange, symbol: &'static str, quantity: Decimal, price: Decimal) -> Self {
            Self {
                exchange,
                symbol,
                quantity,
                price,
                placed: false,
            }
        }
    }

    impl Strategy for OneShot {
        fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
            HashMap::from([(
                EX,
                HashSet::from([SubscriptionKind::BBO {
                    symbol: SYM.to_string(),
                }]),
            )])
        }
        fn on_event(&mut self, _e: &IncomeEvent, _v: StrategyView<'_>) -> Vec<Signal> {
            if self.placed {
                return Vec::new();
            }
            self.placed = true;
            vec![Signal::PlaceOrders {
                orders: vec![OrderIntent {
                    exchange: self.exchange,
                    symbol: self.symbol.to_string(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price: self.price,
                        tif: TimeInForce::PostOnly,
                    },
                    quantity: self.quantity,
                    reduce_only: false,
                }],
                comment: "t".to_string(),
            }]
        }
    }

    fn runner(strategy: OneShot) -> StrategyRunner {
        StrategyRunner::new(
            Box::new(strategy),
            metas(),
            crate::backtest::backtest_account(),
            Box::new(SequentialClientOrderIdGen::default()),
        )
    }

    fn clock() -> IncomeEvent {
        IncomeEvent::market(1, 1, MarketData::Clock)
    }

    fn run_once(quantity: Decimal, price: Decimal) -> Order {
        match runner(OneShot::new(quantity, price))
            .on_event(&clock())
            .expect("无报错")
            .into_iter()
            .next()
            .expect("signal")
        {
            OutcomeEvent::PlaceOrders { orders, .. } => orders.into_iter().next().unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 产出订单必须仍是**币本位**（单位折算是出口的事），但已按交易所精度取整、已赋号。
    /// 这条同时守住回测：`SimState` 拿到的就是这个订单。
    #[test]
    fn emitted_order_is_coin_denominated_and_rounded() {
        // 0.125 币 = 12.5 张 -> 向下取整 12 张 = 0.12 币（**不是** 12）
        let order = run_once(dec!(0.125), dec!(62_500.17));
        assert!(
            order.quantity == dec!(0.12),
            "应为币本位 0.12，若得到 12 说明单位折算错误地留在了 runner 里: {}",
            order.quantity
        );
        match order.order_type {
            OrderType::Limit { price, .. } => assert_eq!(price, dec!(62500.2)),
            OrderType::Market => panic!("expected limit"),
        }
        assert_eq!(order.client_order_id, "bt0");
    }

    /// 跟踪表登记的就是发出的那张单（取整后的量）：拿成交与单量比较时（#796）口径与交易所一致
    #[test]
    fn tracked_order_is_the_rounded_order_sent() {
        let mut runner = runner(OneShot::new(dec!(0.125), dec!(62_500.0)));
        runner.on_event(&clock()).unwrap();
        let state = runner
            .state()
            .symbol_state(&SYM.to_string())
            .expect("state");
        let pending: Vec<(Decimal, &str)> = state
            .order_book()
            .iter()
            .map(|p| (p.order.quantity, p.order.client_order_id.as_str()))
            .collect();
        assert_eq!(pending, vec![(dec!(0.12), "bt0")]);
    }

    /// 下单范围闸门：订阅范围外的 (所, 币) 不发、不登记，报错让驱动方停机
    #[test]
    fn orders_outside_the_subscription_scope_stop_the_engine() {
        for (exchange, symbol) in [(EX, "ETH"), (Exchange::Binance, SYM)] {
            let mut runner = runner(OneShot::at(exchange, symbol, dec!(1), dec!(1)));
            let err = runner.on_event(&clock()).expect_err("范围外下单应报错");
            assert!(err.contains("订阅范围外"), "{err}");
            assert!(runner
                .state()
                .symbol_state(&SYM.to_string())
                .is_some_and(|s| !s.order_book().has_pending()));
        }
    }

    /// 撤范围外的币的单同样停机（与下单同一道闸门）
    #[test]
    fn cancels_outside_the_subscription_scope_stop_the_engine() {
        struct CancelEth;
        impl Strategy for CancelEth {
            fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
                OneShot::new(dec!(1), dec!(1)).public_streams()
            }
            fn on_event(&mut self, _e: &IncomeEvent, _v: StrategyView<'_>) -> Vec<Signal> {
                vec![Signal::CancelOrder {
                    symbol: "ETH".to_string(),
                    client_order_id: "x".to_string(),
                }]
            }
        }
        let mut runner = StrategyRunner::new(
            Box::new(CancelEth),
            metas(),
            crate::backtest::backtest_account(),
            Box::new(SequentialClientOrderIdGen::default()),
        );
        let err = runner.on_event(&clock()).expect_err("范围外撤单应报错");
        assert!(err.contains("订阅范围外"), "{err}");
    }

    /// 范围内但没有合约规格（取不了整）同样停机，不按原值照发
    #[test]
    fn orders_without_symbol_meta_stop_the_engine() {
        let mut runner = StrategyRunner::new(
            Box::new(OneShot::new(dec!(1), dec!(1))),
            Arc::new(HashMap::new()),
            crate::backtest::backtest_account(),
            Box::new(SequentialClientOrderIdGen::default()),
        );
        let err = runner.on_event(&clock()).expect_err("缺规格应报错");
        assert!(err.contains("没有合约规格"), "{err}");
    }

    /// **范围是从 `public_streams()` 推导出来的**。
    ///
    /// 判据本身在 [`SubscriptionScope`] 里测（三档投递范围各一条）；这里测的是接线 ——
    /// runner 把策略声明的订阅正确地变成了范围，尤其是"所级读数只收订了的所"这一档
    /// 依赖的所集合，它是 `public_streams()` 的键集而非另存的副本。
    #[test]
    fn the_scope_is_derived_from_the_strategys_declared_streams() {
        let runner = StrategyRunner::new(
            Box::new(OneShot::new(dec!(1.0), dec!(1.0))),
            metas(),
            crate::domain::AccountId::Live,
            Box::new(SequentialClientOrderIdGen::default()),
        );

        let bbo = |exchange: Exchange, symbol: &str| {
            IncomeEvent::market(
                0,
                0,
                MarketData::BBO(crate::domain::BBO {
                    exchange,
                    symbol: symbol.to_string(),
                    bid_price: dec!(1.0),
                    bid_qty: dec!(1.0),
                    ask_price: dec!(1.0),
                    ask_qty: dec!(1.0),
                    timestamp: 0,
                }),
            )
        };
        let account_info = |exchange: Exchange| {
            IncomeEvent::account(
                crate::domain::AccountId::Live,
                0,
                0,
                crate::messaging::AccountData::Exchange(
                    crate::messaging::ExchangeAccountData::AccountInfo {
                        exchange,
                        equity: 1.0,
                        notional: 0.0,
                    },
                ),
            )
        };

        // 定向档：只收本策略订的 (所, symbol)
        assert!(runner.accepts(&bbo(EX, SYM)));
        assert!(!runner.accepts(&bbo(EX, "ETH")), "未订阅的 symbol 不该收");
        assert!(
            !runner.accepts(&bbo(Exchange::Binance, SYM)),
            "未订阅的所不该收"
        );

        // 所级档：只收本策略订的所
        assert!(runner.accepts(&account_info(EX)));
        assert!(
            !runner.accepts(&account_info(Exchange::Binance)),
            "未订阅所的净值不该到达策略"
        );

        // 广播档：与交易所无关的全局事件一律放行
        assert!(runner.accepts(&IncomeEvent::market(0, 0, MarketData::Clock)));
    }
}

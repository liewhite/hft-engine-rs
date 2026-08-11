//! StrategyRunner —— 策略执行的**纯逻辑核心**。
//!
//! 把一个事件喂给策略、维护策略独享状态、产出策略信号。不含任何
//! 传输/并发设施 (无 actor / channel / pubsub)，因此被两种驱动复用：
//!   - 实盘 [`crate::engine::ExecutorActor`]：actor 串行消费总线事件，调用 [`StrategyRunner::on_event`]
//!     后把结果发布到 OutcomePubSub
//!   - 回测 [`crate::backtest::BacktestEngine`]：单线程虚拟时间循环里同步调用 [`StrategyRunner::accepts`] /
//!     [`StrategyRunner::on_event`]
//!
//! 职责：过滤订阅范围、更新 StateManager、分配 client_order_id、登记 pending、按交易所精度
//! 取整价格与数量。
//!
//! **只取整，不换单位**：产出的订单一律**币本位** (见 [`crate::domain::Quantity`])，但价格与
//! 数量已按交易所精度取整。两件事必须分在两处：
//!   - 精度取整是**市场规则**，回测/模拟盘也必须遵守，否则 `SimState` 会用非法 tick 价与
//!     非法步长撮合，结果失真 —— 故留在此处，两条驱动共享
//!   - 单位折算 (币 -> 张) 是**线路细节**，只发生在实盘出口，见
//!     [`crate::exchange::ExchangeOrder`]；回测不经过那里，因此全程币本位

use crate::domain::{Exchange, Order, OrderType, Symbol, SymbolMeta};
use crate::messaging::{IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// client_order_id 生成策略 (注入点)。
///
/// 生产用交易所规范的随机 id (UUID) 保证跨重启唯一；回测用确定性自增计数，保证
/// "同一输入 -> 同一逐笔回报序列" (client_order_id 会随订单/成交回报投递给策略)。
pub trait ClientOrderIdGen: Send {
    fn next_id(&mut self, exchange: Exchange) -> String;
}

/// 生产默认：交易所规范的随机 client_order_id (UUID)。
#[derive(Default)]
pub struct ExchangeUuidGen;
impl ClientOrderIdGen for ExchangeUuidGen {
    fn next_id(&mut self, exchange: Exchange) -> String {
        exchange.new_cli_order_id()
    }
}

/// 回测确定性：自增计数 (与墙钟/随机无关)。
#[derive(Default)]
pub struct SequentialClientOrderIdGen {
    next: u64,
}
impl ClientOrderIdGen for SequentialClientOrderIdGen {
    fn next_id(&mut self, _exchange: Exchange) -> String {
        let id = format!("bt{}", self.next);
        self.next += 1;
        id
    }
}

pub struct StrategyRunner {
    strategy: Box<dyn Strategy>,
    state: StateManager,
    /// 用于按交易所精度取整（**不**用于单位折算）
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 策略订阅的 (exchange, symbol) 集合，用于事件过滤
    subscriptions: HashSet<(Exchange, Symbol)>,
    /// client_order_id 生成策略 (生产 UUID / 回测确定性计数)
    id_gen: Box<dyn ClientOrderIdGen>,
}

impl StrategyRunner {
    /// 生产构造：client_order_id 用交易所 UUID。
    pub fn new(
        strategy: Box<dyn Strategy>,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    ) -> Self {
        Self::with_id_gen(strategy, symbol_metas, Box::new(ExchangeUuidGen))
    }

    /// 注入 id 生成策略 (回测传 [`SequentialClientOrderIdGen`] 以确保确定性)。
    pub fn with_id_gen(
        strategy: Box<dyn Strategy>,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        id_gen: Box<dyn ClientOrderIdGen>,
    ) -> Self {
        // 从 public_streams 推导订阅的 (exchange, symbol) 集合
        let subscriptions: HashSet<(Exchange, Symbol)> = strategy
            .public_streams()
            .iter()
            .flat_map(|(ex, kinds)| kinds.iter().map(move |k| (*ex, k.symbol().clone())))
            .collect();
        let symbols: Vec<Symbol> = subscriptions.iter().map(|(_, s)| s.clone()).collect();
        let state = StateManager::new(&symbols, strategy.order_timeout_ms());
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

    /// 写入持仓基线（实盘投产握手；回测/模拟账户从 0 起算，不调用）。
    /// 语义见 [`crate::messaging::PositionBaseline`] 与 `SymbolState::seed_position`。
    pub fn seed_positions(&mut self, baselines: &[crate::messaging::PositionBaseline]) {
        self.state.seed_positions(baselines);
    }

    /// 全局事件 (无路由键) 广播；symbol 事件仅接收订阅范围内的。
    pub fn accepts(&self, event: &IncomeEvent) -> bool {
        match event.routing() {
            Some(route) => self.subscriptions.contains(&route),
            None => true,
        }
    }

    /// 更新状态并运行策略，返回信号 (下单已分配 id、登记 pending、按交易所精度取整)。
    ///
    /// 产出订单是**币本位**且已取整；单位折算是出口的职责，见模块文档。
    pub fn on_event(&mut self, event: &IncomeEvent) -> Vec<OutcomeEvent> {
        self.state.apply(event);
        // 单一分发入口：一切事件（含 BorrowFee/ExchangeRate）都走 on_event，
        // 策略自行 match 关心的变体 —— 不存在第二套 typed 回调。
        let raw = self.strategy.on_event(event, &self.state);
        raw
            .into_iter()
            .map(|signal| match signal {
                OutcomeEvent::PlaceOrders { orders, comment } => {
                    let converted = orders
                        .into_iter()
                        .map(|mut order| {
                            order.client_order_id = self.id_gen.next_id(order.exchange);
                            // 登记未取整的原始订单，与取整前的行为保持一致（策略比对自己
                            // 的挂单量时看到的仍是它请求的数量）
                            // created_at 取当前事件时刻 (回测=虚拟时间, 确定性), 不读墙钟
                            self.state.add_pending_order(order.clone(), event.local_ts());
                            self.round_to_exchange_precision(order)
                        })
                        .collect();
                    OutcomeEvent::PlaceOrders {
                        orders: converted,
                        comment,
                    }
                }
                cancel @ OutcomeEvent::CancelOrder { .. } => cancel,
                // 自定义事件原样放行：无订单语义，不登记 pending、不过精度取整
                emit @ OutcomeEvent::Emit(_) => emit,
            })
            .collect()
    }

    /// 按交易所精度取整价格与数量，**保持币本位**。
    ///
    /// 缺少 SymbolMeta 说明策略交易了未预加载的 symbol，仅告警并按原值发出（与改造前一致；
    /// 真正致命的单位问题由出口的 [`crate::exchange::ExchangeOrder`] 兜住 —— 那里缺 meta 会
    /// 直接拒单）。
    fn round_to_exchange_precision(&self, order: Order) -> Order {
        let key = (order.exchange, order.symbol.clone());
        let meta = match self.symbol_metas.get(&key) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    "SymbolMeta not found, order precision not rounded"
                );
                return order;
            }
        };
        let quantity = meta.round_coin_size_down(order.quantity);
        let order_type = match &order.order_type {
            OrderType::Market => OrderType::Market,
            OrderType::Limit { price, tif } => OrderType::Limit {
                price: meta.round_price(*price),
                tif: *tif,
            },
        };
        Order {
            order_type,
            quantity,
            ..order
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{OrderType, Side, TimeInForce};
    use crate::exchange::utils::StepFormatter;
    use crate::exchange::SubscriptionKind;
    use crate::messaging::{IncomeEvent, MarketData, StateManager};
    use crate::strategy::{OutcomeEvent, Strategy};

    const EX: Exchange = Exchange::OKX;
    const SYM: &str = "BTC";
    /// OKX BTC-USDT-SWAP：每张 0.01 币，数量步长 1 张 -> 合法币量是 0.01 的整数倍
    const CONTRACT_SIZE: f64 = 0.01;

    fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        Arc::new(HashMap::from([(
            (EX, SYM.to_string()),
            SymbolMeta {
                exchange: EX,
                symbol: SYM.to_string(),
                price_formatter: Arc::new(StepFormatter::new(0.1)),
                size_step: 1.0,
                min_order_size: 1.0,
                contract_size: CONTRACT_SIZE,
            },
        )]))
    }

    /// 一次性下一张指定数量/价格的挂单
    struct OneShot {
        quantity: f64,
        price: f64,
        placed: bool,
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
        fn order_timeout_ms(&self) -> u64 {
            0
        }
        fn on_event(&mut self, _e: &IncomeEvent, _s: &StateManager) -> Vec<OutcomeEvent> {
            if self.placed {
                return Vec::new();
            }
            self.placed = true;
            vec![OutcomeEvent::PlaceOrders {
                orders: vec![Order {
                    id: String::new(),
                    exchange: EX,
                    symbol: SYM.to_string(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price: self.price,
                        tif: TimeInForce::PostOnly,
                    },
                    quantity: self.quantity,
                    reduce_only: false,
                    client_order_id: String::new(),
                }],
                comment: "t".to_string(),
            }]
        }
    }

    fn run_once(quantity: f64, price: f64) -> Order {
        let mut runner = StrategyRunner::with_id_gen(
            Box::new(OneShot {
                quantity,
                price,
                placed: false,
            }),
            metas(),
            Box::new(SequentialClientOrderIdGen::default()),
        );
        let ev = IncomeEvent::market(1, 1, MarketData::Clock);
        match runner.on_event(&ev).into_iter().next().expect("signal") {
            OutcomeEvent::PlaceOrders { orders, .. } => orders.into_iter().next().unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 产出订单必须仍是**币本位**（单位折算是出口的事），但已按交易所精度取整。
    /// 这条同时守住回测：`SimState` 拿到的就是这个订单。
    #[test]
    fn emitted_order_is_coin_denominated_and_rounded() {
        // 0.125 币 = 12.5 张 -> 向下取整 12 张 = 0.12 币（**不是** 12）
        let order = run_once(0.125, 62_500.17);
        assert!(
            (order.quantity - 0.12).abs() < 1e-12,
            "应为币本位 0.12，若得到 12 说明单位折算错误地留在了 runner 里: {}",
            order.quantity
        );
        match order.order_type {
            OrderType::Limit { price, .. } => assert!((price - 62_500.2).abs() < 1e-9),
            OrderType::Market => panic!("expected limit"),
        }
    }

    /// pending 登记的是策略请求的原始数量（未取整），与改造前行为一致
    #[test]
    fn pending_order_keeps_strategy_requested_quantity() {
        let mut runner = StrategyRunner::with_id_gen(
            Box::new(OneShot {
                quantity: 0.125,
                price: 62_500.0,
                placed: false,
            }),
            metas(),
            Box::new(SequentialClientOrderIdGen::default()),
        );
        let ev = IncomeEvent::market(1, 1, MarketData::Clock);
        runner.on_event(&ev);
        let state = runner.state().symbol_state(&SYM.to_string()).expect("state");
        let pending: Vec<f64> = state.pending_orders().map(|p| p.order.quantity).collect();
        assert_eq!(pending, vec![0.125]);
    }
}

//! 回测引擎集成测试 (对应 ox-demo `BacktestEngineSpec`)：
//! 用内存假数据源驱动，验证 下单->挂单->越价成交 全链路 + 确定性 + trade-print 撮合。

use hft_engine_rs::backtest::{BacktestEngine, BacktestResult, MarketDataSource};
use hft_engine_rs::domain::{
    Exchange, Order, OrderType, Price, Symbol, SymbolMeta, Side, TimeInForce, Timestamp, BBO,
};
use hft_engine_rs::engine::{SequentialClientOrderIdGen, StrategyRunner};
use hft_engine_rs::exchange::utils::StepFormatter;
use hft_engine_rs::exchange::SubscriptionKind;
use hft_engine_rs::messaging::{AccountData, IncomeEvent, MarketData};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::{OutcomeEvent, Strategy, StrategyView};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

const EX: Exchange = Exchange::Binance;
const SYM: &str = "BTCUSDT";

fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
    let meta = SymbolMeta {
        exchange: EX,
        symbol: SYM.to_string(),
        price_formatter: Arc::new(StepFormatter::new(0.01)),
        size_step: 0.001,
        min_order_size: 0.001,
        contract_size: 1.0,
    };
    let mut m = HashMap::new();
    m.insert((EX, SYM.to_string()), meta);
    Arc::new(m)
}

fn bbo_ev(bid: Price, ask: Price, ts: Timestamp) -> IncomeEvent {
    IncomeEvent::market(
        ts,
        ts,
        MarketData::BBO(BBO {
            exchange: EX,
            symbol: SYM.to_string(),
            bid_price: bid,
            bid_qty: 1.0,
            ask_price: ask,
            ask_qty: 1.0,
            timestamp: ts,
        }),
    )
}

fn trade_ev(price: Price, ts: Timestamp) -> IncomeEvent {
    IncomeEvent::market(
        ts,
        ts,
        MarketData::MarketTrade(hft_engine_rs::domain::MarketTrade {
            exchange: EX,
            symbol: SYM.to_string(),
            price,
            qty: 1.0,
            is_buyer_maker: false,
            timestamp: ts,
        }),
    )
}

/// 假数据源：手造事件序列。
struct FixedSource {
    evs: Vec<IncomeEvent>,
}
impl MarketDataSource for FixedSource {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
        Box::new(self.evs.iter().cloned())
    }
}

/// 首个市场事件 (BBO 或 MarketTrade) 时挂一张限价买单。
struct OneShotBuy {
    price: Price,
    tif: TimeInForce,
    placed: bool,
}
impl Strategy for OneShotBuy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let mut m = HashMap::new();
        let mut kinds = HashSet::new();
        kinds.insert(SubscriptionKind::BBO { symbol: SYM.to_string() });
        m.insert(EX, kinds);
        m
    }
    fn order_timeout_ms(&self) -> u64 {
        0
    }
    fn on_event(&mut self, event: &IncomeEvent, _view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        let is_market = matches!(
            event,
            IncomeEvent::Market(m) if matches!(m.data, MarketData::BBO(_) | MarketData::MarketTrade(_))
        );
        if is_market && !self.placed {
            self.placed = true;
            return vec![OutcomeEvent::PlaceOrders {
                orders: vec![Order {
                    id: String::new(),
                    exchange: EX,
                    symbol: SYM.to_string(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price: self.price,
                        tif: self.tif,
                    },
                    quantity: 1.0,
                    reduce_only: false,
                    client_order_id: String::new(),
                }],
                comment: "buy".to_string(),
            }];
        }
        Vec::new()
    }
}

/// 把投递事件压成可比对的签名 (含 client_order_id / 状态 / 价量)，用于逐笔确定性断言。
fn event_sig(ev: &IncomeEvent) -> String {
    match ev {
        IncomeEvent::Account(a) => match &a.data {
            AccountData::OrderUpdate(u) => format!(
                "OU ts={} cid={:?} {:?} qty={}",
                a.exchange_ts, u.client_order_id, u.status, u.quantity
            ),
            AccountData::Fill(f) => format!(
                "FL ts={} cid={:?} oid={} {:?} px={} sz={} fee={}",
                a.exchange_ts, f.client_order_id, f.order_id, f.side, f.price, f.size, f.fee
            ),
            AccountData::AccountInfo { equity, .. } => {
                format!("ACC ts={} eq={}", a.exchange_ts, equity)
            }
            other => format!("{other:?}"),
        },
        IncomeEvent::Market(m) => match &m.data {
            MarketData::BBO(b) => {
                format!("BBO ts={} bid={} ask={}", m.exchange_ts, b.bid_price, b.ask_price)
            }
            other => format!("{other:?}"),
        },
    }
}

/// 运行 BBO 场景，返回 (结果, 投递事件签名序列)。用确定性 id 生成器保证逐笔可复现。
fn run_bbo() -> (BacktestResult, Vec<String>) {
    let source = FixedSource {
        evs: vec![
            bbo_ev(100.0, 100.1, 1000), // 挂买单 @100 (PostOnly, 不可成交 -> resting)
            bbo_ev(99.8, 99.9, 2000),   // ask 99.9 <= 100 -> 越价, resting 买单成交 @100
            bbo_ev(99.7, 99.8, 3000),
        ],
    };
    let runner = StrategyRunner::with_id_gen(
        Box::new(OneShotBuy {
            price: 100.0,
            tif: TimeInForce::PostOnly,
            placed: false,
        }),
        metas(),
        Box::new(SequentialClientOrderIdGen::default()),
    );
    let config = SimConfig {
        initial_balance_usdt: 10_000.0,
        ..SimConfig::default()
    };
    let sink: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let sink_obs = Rc::clone(&sink);
    let result = {
        let mut engine = BacktestEngine::new(&source, vec![runner], config, metas())
            .with_observer(move |ev: &IncomeEvent| sink_obs.borrow_mut().push(event_sig(ev)));
        engine.run()
    };
    let trace = Rc::try_unwrap(sink).unwrap().into_inner();
    (result, trace)
}

#[test]
fn resting_order_crossed_one_fill_pos_plus_one() {
    let (r, _) = run_bbo();
    assert_eq!(r.fills, 1);
    assert_eq!(r.market_events, 3);
    assert_eq!(r.realized_pnl, 0.0); // 只开仓未平仓
    let pos = r.positions.iter().find(|p| p.symbol == SYM).unwrap();
    assert!((pos.size - 1.0).abs() < 1e-9);
    // 未实现 = (mark 99.75 - entry 100) * 1 = -0.25; equity = 10000 - 0.25
    assert!((r.final_equity - (10_000.0 - 0.25)).abs() < 1e-6);
}

#[test]
fn deterministic_same_input_same_result() {
    let (r1, trace1) = run_bbo();
    let (r2, trace2) = run_bbo();
    assert_eq!(r1, r2);
    // 逐笔事件序列 (含 client_order_id) 也必须完全一致 —— 真正验证确定性, 无盲区
    assert_eq!(trace1, trace2);
    // 确认 client_order_id 是确定性计数而非随机 UUID
    assert!(trace1.iter().any(|s| s.contains("cid=Some(\"bt0\")")), "trace: {trace1:?}");
}

#[test]
fn trade_print_matching_fills_resting_limit() {
    // 纯 trades 行情：挂 GTC 买单 @100 (无 BBO -> resting)，成交价 99 < 100 越价 -> 成交 @100。
    let source = FixedSource {
        evs: vec![
            trade_ev(100.0, 1000), // 触发挂单
            trade_ev(99.0, 2000),  // 99 < 100 -> 越价成交
            trade_ev(98.0, 3000),
        ],
    };
    let runner = StrategyRunner::with_id_gen(
        Box::new(OneShotBuy {
            price: 100.0,
            tif: TimeInForce::GTC,
            placed: false,
        }),
        metas(),
        Box::new(SequentialClientOrderIdGen::default()),
    );
    let config = SimConfig {
        initial_balance_usdt: 10_000.0,
        ..SimConfig::default()
    };
    let mut engine = BacktestEngine::new(&source, vec![runner], config, metas());
    let r = engine.run();
    assert_eq!(r.fills, 1);
    assert_eq!(r.market_events, 3);
    let pos = r.positions.iter().find(|p| p.symbol == SYM).unwrap();
    assert!((pos.size - 1.0).abs() < 1e-9);
    // entry 100, 末次成交价估值 98 -> 未实现 (98-100)*1 = -2; equity = 9998
    assert!((r.final_equity - 9_998.0).abs() < 1e-6);
}

/// 每个 BBO 都 emit 一条自定义事件的策略，同时统计自己在 on_event 里收到的 Custom 条数。
struct EmitOnBbo {
    seen_customs: Arc<std::sync::atomic::AtomicU32>,
}
impl Strategy for EmitOnBbo {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let mut m = HashMap::new();
        let mut kinds = HashSet::new();
        kinds.insert(SubscriptionKind::BBO { symbol: SYM.to_string() });
        m.insert(EX, kinds);
        m
    }
    fn order_timeout_ms(&self) -> u64 {
        0
    }
    fn on_event(&mut self, event: &IncomeEvent, _view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        let IncomeEvent::Market(m) = event else {
            return Vec::new();
        };
        match &m.data {
            MarketData::Custom(_) => {
                self.seen_customs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Vec::new()
            }
            MarketData::BBO(b) => vec![OutcomeEvent::Emit(
                hft_engine_rs::messaging::CustomEvent::new(PingSignal { mid: (b.bid_price + b.ask_price) / 2.0 }),
            )],
            _ => Vec::new(),
        }
    }
}

#[derive(Debug)]
struct PingSignal {
    mid: f64,
}

/// **回环不可能**：策略 emit 的自定义事件只到达旁路观察者（等价实盘 Outcome 总线的
/// 外部订阅者），绝不回流给任何策略 —— 即便是无 scope 的广播事件。
/// 若回流，emit-on-event 的策略会形成事件风暴。
#[test]
fn emitted_custom_events_reach_observers_but_never_strategies() {
    let source = FixedSource {
        evs: vec![bbo_ev(100.0, 100.1, 1000), bbo_ev(100.2, 100.3, 2000)],
    };
    let seen_by_strategy = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runner = StrategyRunner::with_id_gen(
        Box::new(EmitOnBbo { seen_customs: Arc::clone(&seen_by_strategy) }),
        metas(),
        Box::new(SequentialClientOrderIdGen::default()),
    );
    let seen_by_observer: Rc<RefCell<Vec<(usize, f64)>>> = Rc::new(RefCell::new(Vec::new()));
    let obs_sink = Rc::clone(&seen_by_observer);
    {
        let mut engine = BacktestEngine::new(&source, vec![runner], SimConfig::default(), metas())
            .with_outcome_observer(move |runner_idx, outcome| {
                if let OutcomeEvent::Emit(c) = outcome {
                    let sig = c.get::<PingSignal>().expect("观察者按类型取回 payload");
                    obs_sink.borrow_mut().push((runner_idx, sig.mid));
                }
            });
        engine.run();
    }
    assert_eq!(
        seen_by_strategy.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "策略 emit 的自定义事件回流进了策略 —— 回环屏障被打破"
    );
    assert_eq!(
        *seen_by_observer.borrow(),
        vec![(0, 100.05), (0, 100.25)],
        "outcome 观察者应逐条收到 emit 的事件（带发出者下标）并能按类型取回内容"
    );
}

/// 记录自己收到的每条 Custom 事件 tag 的探针策略。
struct CustomProbe {
    symbol: Symbol,
    seen: Arc<std::sync::Mutex<Vec<&'static str>>>,
}
impl Strategy for CustomProbe {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let mut m = HashMap::new();
        let mut kinds = HashSet::new();
        kinds.insert(SubscriptionKind::BBO { symbol: self.symbol.clone() });
        m.insert(EX, kinds);
        m
    }
    fn order_timeout_ms(&self) -> u64 {
        0
    }
    fn on_event(&mut self, event: &IncomeEvent, _view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        if let IncomeEvent::Market(m) = event {
            if let MarketData::Custom(c) = &m.data {
                if let Some(Tag(t)) = c.get::<Tag>() {
                    self.seen.lock().unwrap().push(t);
                }
            }
        }
        Vec::new()
    }
}

struct Tag(&'static str);

fn custom_ev(c: hft_engine_rs::messaging::CustomEvent, ts: Timestamp) -> IncomeEvent {
    IncomeEvent::market(ts, ts, MarketData::Custom(c))
}

/// 入向路由端到端：带 scope 的 Custom 事件只到达订阅了该 symbol 的策略，
/// 无 scope 的广播到达所有策略 —— 钉住 `accepts()` 对 Custom 的过滤语义。
#[test]
fn inbound_custom_events_route_by_scope() {
    use hft_engine_rs::messaging::CustomEvent;
    const SYM2: &str = "ETHUSDT";
    let source = FixedSource {
        evs: vec![
            custom_ev(CustomEvent::for_symbol(EX, SYM.to_string(), Tag("btc")), 1000),
            custom_ev(CustomEvent::for_symbol(EX, SYM2.to_string(), Tag("eth")), 2000),
            custom_ev(CustomEvent::new(Tag("all")), 3000),
        ],
    };
    let seen_btc = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_eth = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runners = vec![
        StrategyRunner::with_id_gen(
            Box::new(CustomProbe { symbol: SYM.to_string(), seen: Arc::clone(&seen_btc) }),
            metas(),
            Box::new(SequentialClientOrderIdGen::default()),
        ),
        StrategyRunner::with_id_gen(
            Box::new(CustomProbe { symbol: SYM2.to_string(), seen: Arc::clone(&seen_eth) }),
            metas(),
            Box::new(SequentialClientOrderIdGen::default()),
        ),
    ];
    BacktestEngine::new(&source, runners, SimConfig::default(), metas()).run();
    assert_eq!(
        *seen_btc.lock().unwrap(),
        vec!["btc", "all"],
        "BTC 策略应收到自己 symbol 的定向事件 + 广播事件，收不到别的 symbol 的"
    );
    assert_eq!(*seen_eth.lock().unwrap(), vec!["eth", "all"]);
}

//! 回测引擎集成测试 (对应 ox-demo `BacktestEngineSpec`)：
//! 用内存假数据源驱动，验证 下单->挂单->越价成交 全链路 + 确定性 + trade-print 撮合。

use hft_engine_rs::backtest::{BacktestEngine, BacktestResult, MarketDataSource};
use hft_engine_rs::domain::PriceStep;
use hft_engine_rs::domain::{
    dec, Exchange, OrderIntent, OrderType, Price, Side, Symbol, SymbolMeta, TimeInForce, Timestamp,
    BBO,
};
use hft_engine_rs::engine::{subscription_scope, SequentialClientOrderIdGen, StrategyRunner};
use hft_engine_rs::exchange::SubscriptionKind;
use hft_engine_rs::messaging::SymbolMarketData;
use hft_engine_rs::messaging::{
    AccountData, ExchangeAccountData, IncomeEvent, MarketData, SymbolAccountData,
};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::{Signal, Strategy, StrategyView};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// 与驱动方同样的接线：按策略声明的流算订阅范围（回测账户），再交给 runner
fn bt_runner(strategy: impl Strategy + 'static) -> StrategyRunner {
    let scope = subscription_scope(
        hft_engine_rs::backtest::backtest_account(),
        &strategy.public_streams(),
    );
    StrategyRunner::new(
        Box::new(strategy),
        metas(),
        scope,
        Box::new(SequentialClientOrderIdGen::default()),
    )
}

const EX: Exchange = Exchange::Binance;
const SYM: &str = "BTCUSDT";

fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
    let meta = SymbolMeta {
        exchange: EX,
        symbol: SYM.to_string(),
        price_rule: Arc::new(PriceStep::new(
            hft_engine_rs::domain::PositiveDecimal::new(dec!(0.01)).unwrap(),
        )),
        size_step: hft_engine_rs::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
        min_order_size: hft_engine_rs::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
        min_notional: 0.0,
        contract_size: hft_engine_rs::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
        quote: "USDT".to_string(),
    };
    let mut m = HashMap::new();
    m.insert((EX, SYM.to_string()), meta);
    Arc::new(m)
}

fn bbo_ev(bid: Price, ask: Price, ts: Timestamp) -> IncomeEvent {
    IncomeEvent::market(
        ts,
        ts,
        MarketData::Symbol(SymbolMarketData::BBO(BBO {
            exchange: EX,
            symbol: SYM.to_string(),
            bid_price: bid,
            bid_qty: dec!(1.0),
            ask_price: ask,
            ask_qty: dec!(1.0),
            timestamp: ts,
        })),
    )
}

fn trade_ev(price: Price, ts: Timestamp) -> IncomeEvent {
    IncomeEvent::market(
        ts,
        ts,
        MarketData::Symbol(SymbolMarketData::MarketTrade(
            hft_engine_rs::domain::MarketTrade {
                exchange: EX,
                symbol: SYM.to_string(),
                price,
                qty: dec!(1.0),
                is_buyer_maker: false,
                timestamp: ts,
            },
        )),
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
        kinds.insert(SubscriptionKind::BBO {
            symbol: SYM.to_string(),
        });
        m.insert(EX, kinds);
        m
    }
    fn on_event(&mut self, event: &IncomeEvent, _view: StrategyView<'_>) -> Vec<Signal> {
        let is_market = matches!(
            event,
            IncomeEvent::Market(m) if matches!(m.data, MarketData::Symbol(SymbolMarketData::BBO(_)) | MarketData::Symbol(SymbolMarketData::MarketTrade(_)))
        );
        if is_market && !self.placed {
            self.placed = true;
            return vec![Signal::PlaceOrders {
                orders: vec![OrderIntent {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price: self.price,
                        tif: self.tif,
                    },
                    quantity: dec!(1.0),
                    reduce_only: false,
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
            AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => format!(
                "OU ts={} cid={:?} {:?} qty={}",
                a.exchange_ts, u.client_order_id, u.status, u.quantity
            ),
            AccountData::Symbol(SymbolAccountData::Fill(f)) => format!(
                "FL ts={} cid={:?} oid={} {:?} px={} sz={} fee={}",
                a.exchange_ts,
                f.client_order_id,
                f.order_id,
                f.side,
                f.price,
                f.size,
                f.fee.as_ref().map(|x| x.amount).unwrap_or(0.0)
            ),
            AccountData::Exchange(ExchangeAccountData::Equity { equity, .. }) => {
                format!("ACC ts={} eq={}", a.exchange_ts, equity.amount)
            }
            other => format!("{other:?}"),
        },
        IncomeEvent::Market(m) => match &m.data {
            MarketData::Symbol(SymbolMarketData::BBO(b)) => {
                format!(
                    "BBO ts={} bid={} ask={}",
                    m.exchange_ts, b.bid_price, b.ask_price
                )
            }
            other => format!("{other:?}"),
        },
    }
}

/// 运行 BBO 场景，返回 (结果, 投递事件签名序列)。用确定性 id 生成器保证逐笔可复现。
fn run_bbo() -> (BacktestResult, Vec<String>) {
    let source = FixedSource {
        evs: vec![
            bbo_ev(dec!(100.0), dec!(100.1), 1000), // 挂买单 @100 (PostOnly, 不可成交 -> resting)
            bbo_ev(dec!(99.8), dec!(99.9), 2000), // ask 99.9 <= 100 -> 越价, resting 买单成交 @100
            bbo_ev(dec!(99.7), dec!(99.8), 3000),
        ],
    };
    let runner = bt_runner(OneShotBuy {
        price: dec!(100.0),
        tif: TimeInForce::PostOnly,
        placed: false,
    });
    let config = SimConfig {
        initial_balance: 10_000.0,
        ..SimConfig::default()
    };
    let sink: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let sink_obs = Rc::clone(&sink);
    let result = {
        let mut engine = BacktestEngine::new(&source, vec![runner], config, metas())
            .with_observer(move |ev: &IncomeEvent| sink_obs.borrow_mut().push(event_sig(ev)));
        engine.run().expect("回测不该中止")
    };
    let trace = Rc::try_unwrap(sink).unwrap().into_inner();
    (result, trace)
}

#[test]
fn resting_order_crossed_one_fill_pos_plus_one() {
    let (r, _) = run_bbo();
    assert_eq!(r.fills, 1);
    assert_eq!(r.market_events, 3);
    assert_eq!(r.accounts[0].realized_pnl, 0.0); // 只开仓未平仓
    assert_eq!(r.accounts[0].asset, "USDT");
    let pos = r.positions.iter().find(|p| p.symbol == SYM).unwrap();
    assert_eq!(pos.size, dec!(1));
    // 未实现 = (mark 99.75 - entry 100) * 1 = -0.25; equity = 10000 - 0.25
    assert!((r.accounts[0].final_equity - (10_000.0 - 0.25)).abs() < 1e-6);
}

#[test]
fn deterministic_same_input_same_result() {
    let (r1, trace1) = run_bbo();
    let (r2, trace2) = run_bbo();
    assert_eq!(r1, r2);
    // 逐笔事件序列 (含 client_order_id) 也必须完全一致 —— 真正验证确定性, 无盲区
    assert_eq!(trace1, trace2);
    // 确认 client_order_id 是确定性计数而非随机 UUID
    assert!(
        trace1.iter().any(|s| s.contains("cid=Some(\"bt0\")")),
        "trace: {trace1:?}"
    );
}

#[test]
fn trade_print_matching_fills_resting_limit() {
    // 纯 trades 行情：挂 GTC 买单 @100 (无 BBO -> resting)，成交价 99 < 100 越价 -> 成交 @100。
    let source = FixedSource {
        evs: vec![
            trade_ev(dec!(100.0), 1000), // 触发挂单
            trade_ev(dec!(99.0), 2000),  // 99 < 100 -> 越价成交
            trade_ev(dec!(98.0), 3000),
        ],
    };
    let runner = bt_runner(OneShotBuy {
        price: dec!(100.0),
        tif: TimeInForce::GTC,
        placed: false,
    });
    let config = SimConfig {
        initial_balance: 10_000.0,
        ..SimConfig::default()
    };
    let mut engine = BacktestEngine::new(&source, vec![runner], config, metas());
    let r = engine.run().expect("回测不该中止");
    assert_eq!(r.fills, 1);
    assert_eq!(r.market_events, 3);
    let pos = r.positions.iter().find(|p| p.symbol == SYM).unwrap();
    assert_eq!(pos.size, dec!(1));
    // entry 100, 末次成交价估值 98 -> 未实现 (98-100)*1 = -2; equity = 9998
    assert!((r.accounts[0].final_equity - 9_998.0).abs() < 1e-6);
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
        kinds.insert(SubscriptionKind::BBO {
            symbol: self.symbol.clone(),
        });
        m.insert(EX, kinds);
        m
    }
    fn on_event(&mut self, event: &IncomeEvent, _view: StrategyView<'_>) -> Vec<Signal> {
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
            custom_ev(
                CustomEvent::for_symbol(EX, SYM.to_string(), Tag("btc")),
                1000,
            ),
            custom_ev(
                CustomEvent::for_symbol(EX, SYM2.to_string(), Tag("eth")),
                2000,
            ),
            custom_ev(CustomEvent::new(Tag("all")), 3000),
        ],
    };
    let seen_btc = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_eth = Arc::new(std::sync::Mutex::new(Vec::new()));
    let runners = vec![
        bt_runner(CustomProbe {
            symbol: SYM.to_string(),
            seen: Arc::clone(&seen_btc),
        }),
        bt_runner(CustomProbe {
            symbol: SYM2.to_string(),
            seen: Arc::clone(&seen_eth),
        }),
    ];
    BacktestEngine::new(&source, runners, SimConfig::default(), metas())
        .run()
        .expect("回测不该中止");
    assert_eq!(
        *seen_btc.lock().unwrap(),
        vec!["btc", "all"],
        "BTC 策略应收到自己 symbol 的定向事件 + 广播事件，收不到别的 symbol 的"
    );
    assert_eq!(*seen_eth.lock().unwrap(), vec!["eth", "all"]);
}

/// 记下策略收到的下单请求结论，挂单逻辑同 [`OneShotBuy`]
struct RequestProbe {
    inner: OneShotBuy,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}
impl Strategy for RequestProbe {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        self.inner.public_streams()
    }
    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<Signal> {
        if let IncomeEvent::Account(a) = event {
            if let AccountData::Symbol(SymbolAccountData::OrderRequest(r)) = &a.data {
                self.seen.lock().unwrap().push(format!("{:?}", r.outcome));
            }
        }
        self.inner.on_event(event, view)
    }
}

/// 回测柜台回的下单结论与实盘出口同形（#812）：会吃单的 PostOnly 以「下单请求被拒」回到策略，
/// 跟踪表随之移出这张单（不会一分钟后报卡单）；受理的单先回「下单受理」
#[test]
fn backtest_counter_reports_request_outcomes_like_live() {
    let run = |price: Price| {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let source = FixedSource {
            evs: vec![
                bbo_ev(dec!(100.0), dec!(100.1), 1000),
                bbo_ev(dec!(100.0), dec!(100.1), 2000),
                // 超过卡单期限：被拒的单若还留在跟踪表里，这里会报错中止
                bbo_ev(dec!(100.0), dec!(100.1), 200_000),
            ],
        };
        let runner = bt_runner(RequestProbe {
            inner: OneShotBuy {
                price,
                tif: TimeInForce::PostOnly,
                placed: false,
            },
            seen: Arc::clone(&seen),
        });
        let mut engine = BacktestEngine::new(&source, vec![runner], SimConfig::default(), metas());
        engine.run().expect("回测应跑完");
        let seen = seen.lock().unwrap().clone();
        seen
    };
    let rejected = run(dec!(100.5)); // 高于 ask，会吃单
    assert!(
        rejected.len() == 1 && rejected[0].starts_with("PlaceRejected"),
        "{rejected:?}"
    );
    let placed = run(dec!(99.0));
    assert!(
        placed.len() == 1 && placed[0].starts_with("Placed"),
        "{placed:?}"
    );
}

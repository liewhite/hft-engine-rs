//! funding_arb 策略经生产路径（StrategyRunner：状态更新 → 策略 → 登记挂单 → 精度取整）的场景测试。

use hft_engine_rs::domain::{
    dec, AccountId, Decimal, Exchange, Fill, FillReason, OrderStatus, OrderType, OrderUpdate, Position, SettledFunding,
    Side, PriceStep, SymbolMeta, TimeInForce, BBO,
};
use hft_engine_rs::engine::{SequentialClientOrderIdGen, StrategyRunner};
use hft_engine_rs::messaging::{
    AccountData, CustomEvent, ExchangeAccountData, IncomeEvent, MarketData, OrderRequestResult, PositionBaseline,
    PositionValues, RequestOutcome, SymbolAccountData,
};
use hft_engine_rs::strategy::{FundingArbConfig, FundingArbStrategy, OutcomeEvent, SettledFundingWindow};
use std::collections::HashMap;
use std::sync::Arc;

const BN: Exchange = Exchange::Binance;
const HL: Exchange = Exchange::Hyperliquid;
const OKX: Exchange = Exchange::OKX;
const SYM: &str = "XMR";
const H: u64 = 3600 * 1000;
/// 事件时刻都在窗口之后（窗口 [T0-24h, T0]）
const T0: u64 = 100 * H;

fn config() -> FundingArbConfig {
    serde_json::from_value(serde_json::json!({
        "open_threshold_apr": 0.5, "close_threshold_apr": 0.1, "order_notional": 200.0, "min_order_notional": 11.0,
        "max_account_leverage": 1.5, "max_symbol_leverage": 0.2, "order_distance": 0.001,
        "requote_ratio": 1.0, "hedge_retry_ms": 1000, "min_requote_ms": 0, "max_bbo_age_ms": 5000,
        "max_funding_age_ms": 7200000, "pairs": [{"symbol": SYM}]
    }))
    .unwrap()
}

/// `min_notional`：交易所最小下单金额（默认场景都是 0，只有配置下限 11U 生效）
fn meta(ex: Exchange, min_notional: f64) -> ((Exchange, String), SymbolMeta) {
    // OKX 用粗粒度（一手 1 个币 ≈ 100U），测零头处理；其余所 0.001
    let step = if ex == OKX { dec!(1) } else { dec!(0.001) };
    (
        (ex, SYM.to_string()),
        SymbolMeta {
            exchange: ex,
            symbol: SYM.to_string(),
            price_rule: Arc::new(PriceStep::new(dec!(0.01))),
            size_step: step,
            min_order_size: step,
            min_notional,
            contract_size: dec!(1),
        },
    )
}

struct Sim {
    runner: StrategyRunner,
    now: u64,
    /// 各单已推过的成交（client_order_id → 累计），订单回报默认按它报累计成交
    fills: HashMap<String, Decimal>,
}

impl Sim {
    /// 币安 × HL 两所，盘口 100/100.1、每档 100 个，净值各 10000，带初始持仓
    fn new(bn_pos: Decimal, hl_pos: Decimal) -> Self {
        Self::multi(&[(BN, bn_pos), (HL, hl_pos)])
    }

    fn without_bbo(bn_pos: Decimal, hl_pos: Decimal) -> Self {
        Self::multi_without_bbo(&[(BN, bn_pos), (HL, hl_pos)])
    }

    /// 任意几所（顺序即策略里的所顺序）
    fn multi(legs: &[(Exchange, Decimal)]) -> Self {
        let mut s = Self::multi_without_bbo(legs);
        for &(ex, _) in legs {
            s.bbo(ex);
        }
        s
    }

    fn multi_without_bbo(legs: &[(Exchange, Decimal)]) -> Self {
        Self::with_exchange_min(legs, &[])
    }

    /// 指定某些所的交易所最小下单金额（其余为 0），不推盘口
    fn with_exchange_min(legs: &[(Exchange, Decimal)], mins: &[(Exchange, f64)]) -> Self {
        let mut s = Self::bare_with_min(legs, mins);
        for &(ex, _) in legs {
            s.equity(ex, 10_000.0, 0.0);
        }
        s
    }

    /// 带初始持仓，不推净值、持仓价值和盘口
    fn bare(legs: &[(Exchange, Decimal)]) -> Self {
        Self::bare_with_min(legs, &[])
    }

    fn bare_with_min(legs: &[(Exchange, Decimal)], mins: &[(Exchange, f64)]) -> Self {
        let p = config().params().remove(0);
        let exchanges: Vec<Exchange> = legs.iter().map(|(e, _)| *e).collect();
        let min_of = |ex: Exchange| mins.iter().find(|(e, _)| *e == ex).map_or(0.0, |(_, m)| *m);
        let mut runner = StrategyRunner::new(
            Box::new(FundingArbStrategy::new(p, exchanges.iter().map(|&e| meta(e, min_of(e)).1).collect())),
            Arc::new(exchanges.iter().map(|&e| meta(e, min_of(e))).collect::<HashMap<_, _>>()),
            AccountId::Live,
            Box::new(SequentialClientOrderIdGen::default()),
        );
        let baselines: Vec<PositionBaseline> = legs
            .iter()
            .map(|&(exchange, size)| PositionBaseline {
                position: Position { exchange, symbol: SYM.into(), size },
                snapshot_req_ts: 1,
            })
            .collect();
        runner.seed_positions(&baselines);
        Sim { runner, now: T0, fills: HashMap::new() }
    }

    fn advance(&mut self, ms: u64) {
        self.now += ms;
    }

    fn ev(&mut self, e: IncomeEvent) -> Vec<OutcomeEvent> {
        self.now += 10;
        self.runner.on_event(&e).expect("订单跟踪表不该报错")
    }

    fn bbo(&mut self, ex: Exchange) -> Vec<OutcomeEvent> {
        self.bbo_depth(ex, dec!(100))
    }

    /// 第一档量为 `qty` 的盘口
    fn bbo_depth(&mut self, ex: Exchange, qty: Decimal) -> Vec<OutcomeEvent> {
        self.bbo_quote(ex, dec!(100), dec!(100.1), qty)
    }

    /// 买一 `bid`、卖一 `ask` 的盘口，每档 100 个
    fn bbo_px(&mut self, ex: Exchange, bid: Decimal, ask: Decimal) -> Vec<OutcomeEvent> {
        self.bbo_quote(ex, bid, ask, dec!(100))
    }

    fn bbo_quote(&mut self, ex: Exchange, bid: Decimal, ask: Decimal, qty: Decimal) -> Vec<OutcomeEvent> {
        let b = BBO { exchange: ex, symbol: SYM.into(), bid_price: bid, bid_qty: qty, ask_price: ask, ask_qty: qty, timestamp: 0 };
        let now = self.now;
        self.ev(IncomeEvent::market(now, now, MarketData::BBO(b)))
    }

    /// 某所净值，以及本地持仓账本给出的该所别的币的持仓价值合计 `others`（记在一个别的币名下）
    fn equity(&mut self, ex: Exchange, equity: f64, others: f64) -> Vec<OutcomeEvent> {
        self.account_info(ex, equity);
        self.position_values(ex, &[("OTHER", others)])
    }

    /// 只推净值（交易所账户总持仓价值给个大数：杠杆闸门不该再用它）
    fn account_info(&mut self, ex: Exchange, equity: f64) -> Vec<OutcomeEvent> {
        let now = self.now;
        let info = ExchangeAccountData::AccountInfo { exchange: ex, equity, notional: 1e9 };
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::Exchange(info)))
    }

    /// 本地持仓账本发来的该所各币持仓价值
    fn position_values(&mut self, ex: Exchange, values: &[(&str, f64)]) -> Vec<OutcomeEvent> {
        let now = self.now;
        let values = values.iter().map(|(s, v)| (s.to_string(), *v)).collect();
        let pv = ExchangeAccountData::PositionValues(PositionValues { exchange: ex, values });
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::Exchange(pv)))
    }

    /// 某所过去 24h 每小时一样的费率，使年化 = `apr`
    fn funding(&mut self, ex: Exchange, apr: f64) -> Vec<OutcomeEvent> {
        let start = T0 - 24 * H;
        let w = SettledFundingWindow {
            exchange: ex,
            symbol: SYM.into(),
            window_start: start,
            fetched_at: T0,
            entries: (1..=24)
                .map(|i| SettledFunding { exchange: ex, symbol: SYM.into(), rate: apr / 365.0 / 24.0, settle_time: start + i * H })
                .collect(),
        };
        let now = self.now;
        self.ev(IncomeEvent::market(now, now, MarketData::Custom(CustomEvent::for_symbol(ex, SYM.into(), w))))
    }

    /// 交易所订单回报。累计成交：Filled 报全量，其余报已推过的成交之和（交易所与成交流一致的常态）
    fn confirm(&mut self, o: &hft_engine_rs::domain::Order, status: OrderStatus) -> Vec<OutcomeEvent> {
        let filled = match status {
            OrderStatus::Filled => o.quantity,
            _ => self.fills.get(&o.client_order_id).copied().unwrap_or(Decimal::ZERO),
        };
        self.confirm_filled(o, status, filled)
    }

    /// 交易所订单回报，累计成交由调用方给（模拟终态先于成交到达）
    fn confirm_filled(&mut self, o: &hft_engine_rs::domain::Order, status: OrderStatus, filled: Decimal) -> Vec<OutcomeEvent> {
        let u = OrderUpdate {
            order_id: format!("oid-{}", o.client_order_id),
            client_order_id: Some(o.client_order_id.clone()),
            exchange: o.exchange,
            symbol: SYM.into(),
            side: o.side,
            status,
            quantity: o.quantity,
            filled_quantity: filled,
        };
        let now = self.now;
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::Symbol(SymbolAccountData::OrderUpdate(u))))
    }

    /// 本地 REST 请求的结论
    fn request(&mut self, o: &hft_engine_rs::domain::Order, outcome: RequestOutcome) -> Vec<OutcomeEvent> {
        let r = OrderRequestResult { exchange: o.exchange, symbol: SYM.into(), client_order_id: o.client_order_id.clone(), outcome };
        let now = self.now;
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::Symbol(SymbolAccountData::OrderRequest(r))))
    }

    fn reject(&mut self, o: &hft_engine_rs::domain::Order) -> Vec<OutcomeEvent> {
        self.request(o, RequestOutcome::PlaceRejected { reason: "margin is insufficient".into() })
    }

    fn fill(&mut self, o: &hft_engine_rs::domain::Order, size: Decimal) -> Vec<OutcomeEvent> {
        *self.fills.entry(o.client_order_id.clone()).or_default() += size;
        let f = Fill {
            exchange: o.exchange,
            symbol: SYM.into(),
            side: o.side,
            price: dec!(100),
            size,
            client_order_id: Some(o.client_order_id.clone()),
            order_id: format!("oid-{}", o.client_order_id),
            timestamp: self.now,
            fee: 0.0,
            reason: FillReason::Normal,
        };
        let now = self.now;
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::Symbol(SymbolAccountData::Fill(f))))
    }
}

fn placed(out: &[OutcomeEvent]) -> Vec<hft_engine_rs::domain::Order> {
    out.iter()
        .flat_map(|e| match e {
            OutcomeEvent::PlaceOrders { orders, .. } => orders.clone(),
            _ => Vec::new(),
        })
        .collect()
}

fn cancels(out: &[OutcomeEvent]) -> Vec<Exchange> {
    out.iter()
        .filter_map(|e| match e {
            OutcomeEvent::CancelOrder { exchange, .. } => Some(*exchange),
            _ => None,
        })
        .collect()
}

#[test]
fn no_orders_without_both_funding_windows() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    assert!(placed(&s.funding(BN, 2.0)).is_empty());
    assert!(placed(&s.bbo(BN)).is_empty());
}

#[test]
fn opens_with_two_makers_short_on_high_funding_side() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let out = s.funding(HL, 0.1);
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    let bn = o.iter().find(|o| o.exchange == BN).unwrap();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap();
    assert_eq!(bn.side, Side::Short);
    assert_eq!(hl.side, Side::Long);
    assert!(matches!(bn.order_type, OrderType::Limit { tif: TimeInForce::PostOnly, price } if price == dec!(100.2)));
    assert!(matches!(hl.order_type, OrderType::Limit { tif: TimeInForce::PostOnly, price } if price == dec!(99.9)));
    // 200U / 100.05 = 1.99900…，按 0.001 向下取整
    assert_eq!(bn.quantity, dec!(1.999));
    assert_eq!(hl.quantity, bn.quantity, "两所同量");
    assert!(!bn.reduce_only && !hl.reduce_only);
    // 不重复挂
    assert!(placed(&s.bbo(BN)).is_empty());
}

#[test]
fn below_threshold_holds() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.5);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

#[test]
fn leverage_cap_blocks_opening() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    // HL 账户别的币已有 1.49 倍杠杆，再加 200U 超 1.5
    s.equity(HL, 10_000.0, 14_900.0);
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

/// #759：账户持仓总价值 = 本地账本里别的币之和 + 本币自己的持仓；账本里本币那一项（可能还没含
/// 刚到的成交）不算，交易所轮询的账户总持仓价值也不再用
#[test]
fn account_leverage_sums_other_symbols_from_the_ledger_and_own_position_locally() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    // 账本里本币一项再大也不算（本币用策略自己的持仓），别的币合计 14_700 + 本次 200 = 1.49 倍
    s.position_values(HL, &[(SYM, 1e9), ("OTHER", 10_000.0), ("ANOTHER", 4_700.0)]);
    s.funding(BN, 0.8);
    assert_eq!(placed(&s.funding(HL, 0.1)).len(), 2);
}

#[test]
fn account_leverage_blocks_when_other_symbols_sum_over_the_cap() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    // 两个别的币合计 14_900，再加 200 超 1.5
    s.position_values(HL, &[("OTHER", 10_000.0), ("ANOTHER", 4_900.0)]);
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

#[test]
fn unknown_position_values_block_opening() {
    // 只有净值、账本还没发过持仓价值
    let mut s = Sim::bare(&[(BN, dec!(0.0)), (HL, dec!(0.0))]);
    for ex in [BN, HL] {
        s.account_info(ex, 10_000.0);
        s.bbo(ex);
    }
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty(), "持仓价值未知，不能开仓");
    // HL 有了读数、BN 仍未知：还是不开
    assert!(placed(&s.position_values(HL, &[])).is_empty());
    assert_eq!(placed(&s.position_values(BN, &[])).len(), 2);
}

#[test]
fn one_leg_fill_cancels_other_then_hedges_the_lagging_leg() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);

    // 币安卖单部分成交 1.0：撤两所挂单，挂单未撤完前不补
    let out = s.fill(&bn, dec!(1.0));
    s.confirm(&bn, OrderStatus::PartiallyFilled);
    let mut c = cancels(&out);
    c.sort();
    assert_eq!(c, vec![BN, HL], "{out:?}");
    assert!(placed(&out).is_empty(), "挂单还在，不能补：{out:?}");
    assert!(cancels(&s.bbo(HL)).is_empty(), "撤单在途不重发");

    // HL 撤掉了，币安那张（还可能再成交）没撤完：不补
    let out = s.confirm(&hl, OrderStatus::Cancelled);
    assert!(placed(&out).is_empty(), "币安挂单还开着，不能补：{out:?}");
    // 币安也撤掉、成交都已入账 → 立刻在 HL 市价买 1.0（开仓加短腿），不用等时间窗口
    let out = s.confirm(&bn, OrderStatus::Cancelled);
    let h = placed(&out);
    assert_eq!(h.len(), 1, "{out:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity, h[0].reduce_only), (HL, Side::Long, dec!(1.0), false));
    assert!(matches!(h[0].order_type, OrderType::Market));

    // 补腿单在途，不重复补
    assert!(placed(&s.bbo(HL)).is_empty());
}

/// #796：一腿全部成交、成交先于终态到：只撤另一腿，不对已成交的单发撤单；终态到、另一腿撤掉后补腿
#[test]
fn full_fill_before_terminal_cancels_only_the_other_leg() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);

    let out = s.fill(&bn, bn.quantity);
    assert_eq!(cancels(&out), vec![HL], "{out:?}");
    assert!(placed(&out).is_empty(), "{out:?}");

    s.confirm(&bn, OrderStatus::Filled);
    let h = placed(&s.confirm(&hl, OrderStatus::Cancelled));
    assert_eq!(h.len(), 1);
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity), (HL, Side::Long, bn.quantity));
}

#[test]
fn reversal_closes_with_reduce_only_makers() {
    let mut s = Sim::new(dec!(-2.0), dec!(2.0)); // 币安空 HL 多
    s.funding(BN, 0.1);
    let out = s.funding(HL, 0.2); // HL 年化更高 → 反转
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    for x in &o {
        assert!(x.reduce_only);
        let want = if x.exchange == BN { Side::Long } else { Side::Short };
        assert_eq!(x.side, want);
        assert_eq!(x.quantity, dec!(1.999));
    }
}

#[test]
fn startup_imbalance_halts_instead_of_trading() {
    // 启动时两腿差 1（可能有不归本策略的仓位）：不补、不开
    let mut s = Sim::without_bbo(dec!(-2.0), dec!(3.0));
    s.bbo(BN);
    assert!(placed(&s.bbo(HL)).is_empty());
    s.funding(BN, 0.1);
    assert!(placed(&s.funding(HL, 0.9)).is_empty());
}

#[test]
fn hl_terminal_before_fill_does_not_double_hedge() {
    // HL maker 的「已成交」终态先到、成交回报晚到：不按旧持仓补，成交到了两腿已平
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&bn, dec!(1.998));
    s.confirm(&bn, OrderStatus::Filled);
    s.confirm_filled(&hl, OrderStatus::Filled, dec!(1.998)); // HL 也成交了，成交回报还没到
    s.advance(5_000);
    let out = s.bbo(HL);
    assert!(placed(&out).is_empty(), "成交没到齐，不补也不重挂：{out:?}");
    let out = s.fill(&hl, dec!(1.998)); // 回报到达，两腿已平
    assert!(placed(&out).iter().all(|o| !matches!(o.order_type, OrderType::Market)), "{out:?}");
    assert!(placed(&s.bbo(HL)).iter().all(|o| !matches!(o.order_type, OrderType::Market)));
}

/// **#758 的漏洞**：HL 买单的「已成交」终态先到、成交晚到时，此前挂单列表一空、持仓还是 0，策略会在
/// HL 立刻再挂一张买单 —— 成交到了之后那张也成交就多出一笔。现在单留在跟踪表里直到成交到齐：
/// 不在 HL 重挂，先撤币安那张；成交到了、币安撤完再在币安市价卖补腿。
#[test]
fn terminal_before_fill_does_not_requote_on_the_same_exchange() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);

    let out = s.confirm_filled(&hl, OrderStatus::Filled, hl.quantity);
    assert!(placed(&out).is_empty(), "成交没到，不能在 HL 再挂：{out:?}");
    assert_eq!(cancels(&out), vec![BN], "那笔成交到了就是不平衡，先撤币安：{out:?}");
    assert!(placed(&s.bbo(HL)).is_empty());

    let out = s.fill(&hl, hl.quantity);
    assert!(placed(&out).is_empty(), "币安的单还没撤完：{out:?}");
    let h = placed(&s.confirm(&bn, OrderStatus::Cancelled));
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity), (BN, Side::Short, hl.quantity));
    assert!(matches!(h[0].order_type, OrderType::Market));
}

/// 下单请求还没结论的单先不撤（#769：HL 的撤单会先于下单到达、报「never placed」，单在盘口多挂一个
/// 重撤间隔），受理的结论一到就撤
#[test]
fn unconfirmed_maker_is_cancelled_once_placed() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    assert_eq!(o.len(), 2);
    let out = s.funding(BN, 0.1); // 不再值得开
    assert!(cancels(&out).is_empty(), "下单还没结论，不发撤单：{out:?}");
    for x in &o {
        let out = s.request(x, RequestOutcome::Placed { order_id: format!("oid-{}", x.client_order_id) });
        assert_eq!(cancels(&out), vec![x.exchange], "{out:?}");
    }
}

/// #769：杠杆按该所自己的盘口价算该所持仓价值，不用几所的平均价。
/// 币安 100、HL 90（均价约 95）；币安空 17.9 再卖 2.1：按 100 算 2000U 以上超 0.2，按均价算放行
#[test]
fn leverage_uses_each_exchanges_own_price() {
    let mut s = Sim::new(dec!(-17.9), dec!(17.9));
    s.bbo_px(HL, dec!(90.0), dec!(90.1));
    s.funding(BN, 0.8);
    let out = s.funding(HL, 0.1);
    assert!(placed(&out).is_empty(), "{out:?}");
}

/// #769：挂单后盘口小跳让杠杆闸门翻成「超限」时，已挂的单在价格容差内照留，不撤也不新挂。
/// 此前闸门一翻就撤、再翻回来又挂，几毫秒内挂了又撤（实盘 XMR 卡在单币 0.2 边上）
#[test]
fn leverage_gate_flipping_keeps_resting_makers() {
    // 两所各 17.99，再加 1.998：按 100.05 算 1999.8U（放行），按 100.07 算 2000.2U（超 0.2）
    let mut s = Sim::new(dec!(-17.99), dec!(17.99));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    assert_eq!(o.len(), 2);
    for x in &o {
        s.confirm(x, OrderStatus::Pending);
    }
    for ex in [BN, HL] {
        let out = s.bbo_px(ex, dec!(100.02), dec!(100.12));
        assert!(cancels(&out).is_empty() && placed(&out).is_empty(), "{out:?}");
    }
    // 超限期间挂单没了（撤掉或价格偏出容差），不再新挂
    let bn = o.iter().find(|x| x.exchange == BN).unwrap();
    let out = s.confirm(bn, OrderStatus::Cancelled);
    assert!(placed(&out).is_empty(), "{out:?}");
}

#[test]
fn failed_cancel_is_retried() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    for x in &o {
        s.confirm(x, OrderStatus::Pending);
    }
    // 资金费反转到持有以下 → 不挂，撤两张
    let out = s.funding(BN, 0.1);
    assert_eq!(cancels(&out).len(), 2, "{out:?}");
    // 撤单在途：不重发
    s.advance(1_100);
    assert!(cancels(&s.bbo(BN)).is_empty());
    // 撤单失败、单还开着：间隔内不重撤，过了间隔重撤
    for x in &o {
        s.request(x, RequestOutcome::CancelFailed { reason: "busy".into() });
    }
    assert!(cancels(&s.bbo(BN)).is_empty());
    s.advance(1_100);
    assert_eq!(cancels(&s.bbo(BN)).len(), 2);
}

#[test]
fn dust_imbalance_is_ignored() {
    // 差 0.1 × 100 = 10U < 最小单额 11U：不补
    let mut s = Sim::new(dec!(-2.0), dec!(2.1));
    assert!(placed(&s.bbo(BN)).is_empty());
}

// ---------------------------------------------------------------- 多所（#673）

#[test]
fn three_exchanges_open_on_widest_pair_only() {
    let mut s = Sim::multi(&[(BN, dec!(0.0)), (HL, dec!(0.0)), (OKX, dec!(0.0))]);
    s.funding(BN, 0.2);
    s.funding(HL, 0.0);
    let out = s.funding(OKX, 0.9);
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    let sell = o.iter().find(|o| o.side == Side::Short).unwrap();
    let buy = o.iter().find(|o| o.side == Side::Long).unwrap();
    assert_eq!((sell.exchange, buy.exchange), (OKX, HL), "年化最高的 OKX 卖、最低的 HL 买，币安不动");
}

#[test]
fn three_exchanges_wait_for_every_funding_before_opening() {
    let mut s = Sim::multi(&[(BN, dec!(0.0)), (HL, dec!(0.0)), (OKX, dec!(0.0))]);
    s.funding(BN, 0.9);
    assert!(placed(&s.funding(HL, 0.0)).is_empty(), "OKX 年化未知时不知道哪对差最大");
}

#[test]
fn close_below_threshold_leaves_third_exchange_alone() {
    // 币安空 2、HL 多 1、OKX 多 1；BN−HL 收益 5% < 10% → 平 HL×BN，OKX×BN 收益 20% 不动
    let mut s = Sim::multi(&[(BN, dec!(-2.0)), (HL, dec!(1.0)), (OKX, dec!(1.0))]);
    s.funding(OKX, -0.1);
    assert!(placed(&s.funding(BN, 0.1)).is_empty(), "OKX×BN 收益 20% ≥ 门槛");
    // 平仓只看这对两所的年化：HL 一到就平
    let out = s.funding(HL, 0.05);
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    assert!(o.iter().all(|x| x.reduce_only && x.exchange != OKX), "{o:?}");
    let hl = o.iter().find(|x| x.exchange == HL).unwrap();
    assert_eq!(hl.side, Side::Short);
    assert_eq!(hl.quantity, dec!(1), "可平量 = min(HL 多 1, 币安空 2)");
}

#[test]
fn pair_earning_above_close_threshold_is_held() {
    // 币安空 HL 多，收益 15%：高于平仓门槛、低于开仓门槛 → 不动
    let mut s = Sim::new(dec!(-2.0), dec!(2.0));
    s.funding(BN, 0.15);
    assert!(placed(&s.funding(HL, 0.0)).is_empty());
}

#[test]
fn three_exchanges_hedge_on_the_quoted_pair() {
    let mut s = Sim::multi(&[(BN, dec!(0.0)), (HL, dec!(0.0)), (OKX, dec!(0.0))]);
    s.funding(BN, 0.2);
    s.funding(HL, 0.0);
    let o = placed(&s.funding(OKX, 0.9));
    let okx = o.iter().find(|o| o.exchange == OKX).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&okx, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&okx, dec!(1.0));
    s.confirm(&okx, OrderStatus::Cancelled);
    let h = placed(&s.confirm(&hl, OrderStatus::Cancelled)); // 两张都结算完即补
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity), (HL, Side::Long, dec!(1.0)), "OKX 卖成交 → 在 HL 买补，不碰币安");
    assert_eq!(okx.quantity, dec!(1), "两腿同量：按 OKX 一手 1 个币向下取整");
    assert_eq!(hl.quantity, dec!(1));
    assert!(matches!(h[0].order_type, OrderType::Market));
}

#[test]
fn both_close_legs_filling_needs_no_hedge() {
    // 平仓：HL 多 2 / 币安空 2，两边挂单各成交 1 → 仍对冲，不补腿
    let mut s = Sim::new(dec!(-2.0), dec!(2.0));
    s.funding(BN, -0.1);
    let o = placed(&s.funding(HL, 0.0));
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    assert_eq!((hl.side, bn.side), (Side::Short, Side::Long));
    s.confirm(&hl, OrderStatus::Pending);
    s.confirm(&bn, OrderStatus::Pending);
    // 两边都成交 1（各平 1），持仓 BN −1 / HL 1，已对冲：不补
    s.fill(&hl, dec!(1.0));
    s.fill(&bn, dec!(1.0));
    s.confirm(&hl, OrderStatus::Cancelled);
    s.confirm(&bn, OrderStatus::Cancelled);
    s.advance(600);
    assert!(placed(&s.bbo(BN)).iter().all(|o| !matches!(o.order_type, OrderType::Market)));
}

#[test]
fn imbalance_below_one_lot_of_the_hedge_exchange_is_dust() {
    // OKX 一手 1 个币（≈100U）。HL 买单先成交 0.5（50U ≥ 最小单额，但 OKX 下不出 0.5）→ 不补、不停机；
    // HL 补满到 1.0 后在 OKX 补卖 1
    let mut s = Sim::multi(&[(HL, dec!(0.0)), (OKX, dec!(0.0))]);
    s.funding(HL, 0.0);
    let o = placed(&s.funding(OKX, 0.9));
    let okx = o.iter().find(|o| o.exchange == OKX).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&okx, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::PartiallyFilled);
    s.fill(&hl, dec!(0.5));
    s.advance(600);
    let out = s.bbo(HL);
    assert!(placed(&out).iter().all(|o| !matches!(o.order_type, OrderType::Market)), "零头不补：{out:?}");
    let c = cancels(&s.fill(&hl, dec!(0.5)));
    assert!(c.contains(&OKX), "差够一手：撤挂单准备补腿：{c:?}");
    s.confirm(&hl, OrderStatus::Filled);
    let h = placed(&s.confirm(&okx, OrderStatus::Cancelled));
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity), (OKX, Side::Short, dec!(1.0)));
}

#[test]
fn hedge_larger_than_target_position_is_not_reduce_only() {
    // 币安留着 0.05 个多仓零头（5U）。在「币安卖 / HL 买」开仓，HL 买单先成交 1.998：
    // 要在币安市价卖 1.998，远超 0.05 的多仓 → 不能设只减仓（否则被交易所截短或拒绝）
    let mut s = Sim::new(dec!(0.05), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&hl, hl.quantity);
    s.confirm(&hl, OrderStatus::Filled);
    let h = placed(&s.confirm(&bn, OrderStatus::Cancelled));
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side), (BN, Side::Short));
    assert!(!h[0].reduce_only, "补腿量 {} 大于币安多仓 0.05", h[0].quantity);
}

#[test]
fn missing_bbo_on_unused_exchange_does_not_block_hedging() {
    // OKX 迟迟没盘口（如品种暂停）：币安×HL 照常开仓（OKX 年化也得已知才开 → 这里给 OKX 年化）
    let mut s = Sim::multi_without_bbo(&[(BN, dec!(0.0)), (HL, dec!(0.0)), (OKX, dec!(0.0))]);
    s.bbo(BN);
    s.bbo(HL);
    s.funding(OKX, 0.3);
    s.funding(HL, 0.0);
    let o = placed(&s.funding(BN, 0.8));
    assert_eq!(o.len(), 2, "{o:?}");
    assert!(o.iter().all(|x| x.exchange != OKX));
}

#[test]
fn thin_book_keeps_resting_makers_instead_of_churning() {
    // 挂上之后对手盘第一档变薄到不够一笔最小单：不撤已挂的单（价格没偏），也不新挂；
    // 盘口恢复后照常
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    assert_eq!(o.len(), 2);
    for x in &o {
        s.confirm(x, OrderStatus::Pending);
    }
    // 测试配置最短挂单时间为 0：去留只看价格容差
    let out = s.bbo_depth(BN, dec!(0.05));
    assert!(cancels(&out).is_empty() && placed(&out).is_empty(), "{out:?}");
    let out = s.bbo(BN);
    assert!(cancels(&out).is_empty() && placed(&out).is_empty(), "{out:?}");
}

#[test]
fn thin_book_does_not_place_new_makers() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.bbo_depth(BN, dec!(0.05));
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

#[test]
fn unmapped_fill_is_ignored() {
    // 账户在品种目录外的成交（如 HL 别的市场、现货）：不检测、照常交易（#760 用户定）
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    let now = s.now;
    s.ev(IncomeEvent::account(
        AccountId::Live,
        now,
        now,
        AccountData::Exchange(ExchangeAccountData::UnmappedFill(hft_engine_rs::messaging::UnmappedFill {
            exchange: HL,
            instrument: "flx:TSLA".into(),
            side: Side::Long,
            size: dec!(1),
            price: dec!(100),
            timestamp: now,
        })),
    ));
    s.funding(BN, 0.8);
    assert_eq!(placed(&s.funding(HL, 0.1)).len(), 2, "目录外成交后照常开仓");
}

// ---------------------------------------------------------------- 交易所最小下单金额（#696）

#[test]
fn exchange_min_notional_above_config_blocks_thin_quotes() {
    // 币安该币最小下单金额 50U（高于配置 11U）。对手盘第一档只有 0.3 个（30U）：
    // 配置下限够，但币安会拒 → 不挂
    let mut s = Sim::with_exchange_min(&[(BN, dec!(0.0)), (HL, dec!(0.0))], &[(BN, 50.0)]);
    s.bbo_depth(BN, dec!(0.3));
    s.bbo(HL);
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
    // 盘口厚了照常挂
    let o = placed(&s.bbo(BN));
    assert_eq!(o.len(), 2, "{o:?}");
}

#[test]
fn imbalance_below_the_hedge_exchanges_min_notional_is_dust() {
    // 币安最小 50U。开仓后 HL 买单先成交 0.3（30U ≥ 配置 11U，但补腿要在币安卖 30U，会被拒）→ 不补
    let mut s = Sim::with_exchange_min(&[(BN, dec!(0.0)), (HL, dec!(0.0))], &[(BN, 50.0)]);
    s.bbo(BN);
    s.bbo(HL);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::PartiallyFilled);
    s.fill(&hl, dec!(0.3));
    s.advance(600);
    let out = s.bbo(HL);
    assert!(placed(&out).iter().all(|o| !matches!(o.order_type, OrderType::Market)), "低于币安下限不补：{out:?}");
    // 再成交 0.3（共 60U）够币安下限 → 撤挂单准备补腿
    let c = cancels(&s.fill(&hl, dec!(0.3)));
    assert!(c.contains(&BN), "{c:?}");
}

/// 拒单退避：某所的 maker 被拒后不在下一个盘口事件立刻重挂，等 hedge_retry_ms；连续被拒等待翻倍；
/// 有一张被接下（挂上）后清零。另一所不受影响。
#[test]
fn rejected_maker_backs_off_per_exchange() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&hl, OrderStatus::Pending);

    // 第一次被拒：1000ms 内不重挂
    s.reject(&bn);
    assert!(placed(&s.bbo(BN)).is_empty(), "刚被拒就重挂了");
    s.advance(1000);
    let again = placed(&s.bbo(BN));
    assert_eq!(again.len(), 1, "退避到期应重挂");
    assert_eq!(again[0].exchange, BN);

    // 连续第二次被拒：等待翻倍到 2000ms
    s.confirm(&again[0], OrderStatus::Rejected { reason: "EXPIRED".into() });
    s.advance(1500);
    assert!(placed(&s.bbo(BN)).is_empty(), "第二次被拒应等 2000ms");
    s.advance(500);
    let third = placed(&s.bbo(BN));
    assert_eq!(third.len(), 1);

    // 被接下后清零：之后再被拒又从 1000ms 起
    s.confirm(&third[0], OrderStatus::Pending);
    s.confirm(&third[0], OrderStatus::Rejected { reason: "x".into() });
    s.advance(1000);
    assert_eq!(placed(&s.bbo(BN)).len(), 1, "被接下后退避应清零");
}

/// 看板读数：各所年化、年化差、当前信号（开仓在哪对所）、未停机
#[test]
fn gauges_report_aprs_spread_and_signal() {
    let mut s = Sim::new(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    s.funding(HL, 0.1);
    let g = s.runner.strategy_gauges(s.now);
    let value = |name: &str, labels: &[(&str, &str)]| {
        g.iter()
            .find(|x| x.name == name && labels.iter().all(|(k, v)| x.labels.iter().any(|(a, b)| a == k && b == v)))
            .map(|x| x.value)
    };
    let bn = BN.to_string();
    let hl = HL.to_string();
    assert!((value("funding_arb_apr", &[("exchange", &bn)]).unwrap() - 0.8).abs() < 1e-9, "{g:?}");
    assert!((value("funding_arb_apr", &[("exchange", &hl)]).unwrap() - 0.1).abs() < 1e-9, "{g:?}");
    assert!((value("funding_arb_apr_spread", &[("symbol", SYM)]).unwrap() - 0.7).abs() < 1e-9, "{g:?}");
    assert_eq!(value("funding_arb_signal", &[("action", "open"), ("sell", &bn), ("buy", &hl)]), Some(1.0), "{g:?}");
    assert_eq!(value("funding_arb_halted", &[("symbol", SYM)]), Some(0.0));
}

/// 盘口年龄：来过盘口的所按最后一条算；从没来过的所从策略收到第一条事件算起（启动起就静默的订阅也有读数；#767）
#[test]
fn gauges_report_bbo_age_including_never_seen_exchanges() {
    let mut s = Sim::without_bbo(dec!(0.0), dec!(0.0));
    s.funding(BN, 0.8);
    let first = s.now;
    s.advance(5_000);
    s.bbo(BN);
    let bn_at = s.now;
    s.advance(2_000);
    let g = s.runner.strategy_gauges(s.now);
    let age = |ex: Exchange| {
        let ex = ex.to_string();
        g.iter()
            .find(|x| x.name == "funding_arb_bbo_age_seconds" && x.labels.iter().any(|(k, v)| k == "exchange" && *v == ex))
            .map(|x| x.value)
    };
    // 事件的时刻取发事件前的 now（Sim::ev 发完再走 10ms）
    assert_eq!(age(BN), Some((s.now - (bn_at - 10)) as f64 / 1000.0), "{g:?}");
    assert!(age(HL).is_some_and(|a| a >= (s.now - first) as f64 / 1000.0), "从没来过盘口的所也要有读数，且不短于第一条事件以来的时长: {g:?}");
}

//! funding_arb 策略经生产路径（StrategyRunner：状态更新 → 策略 → 登记挂单 → 精度取整）的场景测试。

use hft_engine_rs::domain::{
    AccountId, Exchange, Fill, FillReason, OrderStatus, OrderType, OrderUpdate, Position, SettledFunding,
    Side, StepFormatter, SymbolMeta, TimeInForce, BBO,
};
use hft_engine_rs::engine::StrategyRunner;
use hft_engine_rs::messaging::{AccountData, CustomEvent, IncomeEvent, MarketData, PositionBaseline};
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
        "requote_ratio": 1.0, "hedge_retry_ms": 1000, "fill_settle_ms": 500, "min_requote_ms": 0, "max_bbo_age_ms": 5000,
        "max_funding_age_ms": 7200000, "pairs": [{"symbol": SYM}]
    }))
    .unwrap()
}

fn meta(ex: Exchange) -> ((Exchange, String), SymbolMeta) {
    // OKX 用粗粒度（一手 1 个币 ≈ 100U），测零头处理；其余所 0.001
    let step = if ex == OKX { 1.0 } else { 0.001 };
    (
        (ex, SYM.to_string()),
        SymbolMeta {
            exchange: ex,
            symbol: SYM.to_string(),
            price_formatter: Arc::new(StepFormatter::new(0.01)),
            size_step: step,
            min_order_size: step,
            contract_size: 1.0,
        },
    )
}

struct Sim {
    runner: StrategyRunner,
    now: u64,
}

impl Sim {
    /// 币安 × HL 两所，盘口 100/100.1、每档 100 个，净值各 10000，带初始持仓
    fn new(bn_pos: f64, hl_pos: f64) -> Self {
        Self::multi(&[(BN, bn_pos), (HL, hl_pos)])
    }

    fn without_bbo(bn_pos: f64, hl_pos: f64) -> Self {
        Self::multi_without_bbo(&[(BN, bn_pos), (HL, hl_pos)])
    }

    /// 任意几所（顺序即策略里的所顺序）
    fn multi(legs: &[(Exchange, f64)]) -> Self {
        let mut s = Self::multi_without_bbo(legs);
        for &(ex, _) in legs {
            s.bbo(ex);
        }
        s
    }

    fn multi_without_bbo(legs: &[(Exchange, f64)]) -> Self {
        let p = config().params().remove(0);
        let exchanges: Vec<Exchange> = legs.iter().map(|(e, _)| *e).collect();
        let mut runner = StrategyRunner::new(
            Box::new(FundingArbStrategy::new(p, exchanges.iter().map(|&e| meta(e).1).collect())),
            Arc::new(exchanges.iter().map(|&e| meta(e)).collect::<HashMap<_, _>>()),
        );
        let baselines: Vec<PositionBaseline> = legs
            .iter()
            .map(|&(exchange, size)| PositionBaseline {
                position: Position { exchange, symbol: SYM.into(), size },
                snapshot_req_ts: 1,
            })
            .collect();
        runner.seed_positions(&baselines);
        let mut s = Sim { runner, now: T0 };
        for ex in exchanges {
            s.equity(ex, 10_000.0, 0.0);
        }
        s
    }

    fn advance(&mut self, ms: u64) {
        self.now += ms;
    }

    fn ev(&mut self, e: IncomeEvent) -> Vec<OutcomeEvent> {
        self.now += 10;
        self.runner.on_event(&e)
    }

    fn bbo(&mut self, ex: Exchange) -> Vec<OutcomeEvent> {
        self.bbo_depth(ex, 100.0)
    }

    /// 第一档量为 `qty` 的盘口
    fn bbo_depth(&mut self, ex: Exchange, qty: f64) -> Vec<OutcomeEvent> {
        let b = BBO { exchange: ex, symbol: SYM.into(), bid_price: 100.0, bid_qty: qty, ask_price: 100.1, ask_qty: qty, timestamp: 0 };
        let now = self.now;
        self.ev(IncomeEvent::market(now, now, MarketData::BBO(b)))
    }

    fn equity(&mut self, ex: Exchange, equity: f64, notional: f64) -> Vec<OutcomeEvent> {
        let now = self.now;
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::AccountInfo { exchange: ex, equity, notional }))
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

    fn confirm(&mut self, o: &hft_engine_rs::domain::Order, status: OrderStatus) -> Vec<OutcomeEvent> {
        let u = OrderUpdate {
            order_id: format!("oid-{}", o.client_order_id),
            client_order_id: Some(o.client_order_id.clone()),
            exchange: o.exchange,
            symbol: SYM.into(),
            side: o.side,
            status,
            quantity: o.quantity,
        };
        let now = self.now;
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::OrderUpdate(u)))
    }

    fn fill(&mut self, o: &hft_engine_rs::domain::Order, size: f64) -> Vec<OutcomeEvent> {
        let f = Fill {
            exchange: o.exchange,
            symbol: SYM.into(),
            side: o.side,
            price: 100.0,
            size,
            client_order_id: Some(o.client_order_id.clone()),
            order_id: format!("oid-{}", o.client_order_id),
            timestamp: self.now,
            fee: 0.0,
            reason: FillReason::Normal,
        };
        let now = self.now;
        self.ev(IncomeEvent::account(AccountId::Live, now, now, AccountData::Fill(f)))
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
    let mut s = Sim::new(0.0, 0.0);
    assert!(placed(&s.funding(BN, 2.0)).is_empty());
    assert!(placed(&s.bbo(BN)).is_empty());
}

#[test]
fn opens_with_two_makers_short_on_high_funding_side() {
    let mut s = Sim::new(0.0, 0.0);
    s.funding(BN, 0.8);
    let out = s.funding(HL, 0.1);
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    let bn = o.iter().find(|o| o.exchange == BN).unwrap();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap();
    assert_eq!(bn.side, Side::Short);
    assert_eq!(hl.side, Side::Long);
    assert!(matches!(bn.order_type, OrderType::Limit { tif: TimeInForce::PostOnly, price } if (price - 100.2).abs() < 0.011));
    assert!(matches!(hl.order_type, OrderType::Limit { tif: TimeInForce::PostOnly, price } if (price - 99.9).abs() < 0.011));
    // 200U / 100.05 ≈ 1.998
    assert!((bn.quantity - 1.998).abs() < 0.002, "{}", bn.quantity);
    assert!(!bn.reduce_only && !hl.reduce_only);
    // 不重复挂
    assert!(placed(&s.bbo(BN)).is_empty());
}

#[test]
fn below_threshold_holds() {
    let mut s = Sim::new(0.0, 0.0);
    s.funding(BN, 0.5);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

#[test]
fn leverage_cap_blocks_opening() {
    let mut s = Sim::new(0.0, 0.0);
    // HL 账户已有 1.49 倍杠杆，再加 200U 超 1.5
    s.equity(HL, 10_000.0, 14_900.0);
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

#[test]
fn one_leg_fill_cancels_other_then_hedges_the_lagging_leg() {
    let mut s = Sim::new(0.0, 0.0);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);

    // 币安卖单部分成交 1.0：撤两所挂单，HL 挂单未撤完前不补
    s.confirm(&bn, OrderStatus::PartiallyFilled);
    let out = s.fill(&bn, 1.0);
    let mut c = cancels(&out);
    c.sort();
    assert_eq!(c, vec![BN, HL], "{out:?}");
    assert!(placed(&out).is_empty(), "HL 挂单还在，不能补：{out:?}");

    // HL 挂单撤掉：它的成交回报可能还在路上，结算窗口内不补
    let out = s.confirm(&hl, OrderStatus::Cancelled);
    assert!(placed(&out).is_empty(), "结算窗口内不能补：{out:?}");
    // 过了窗口 → 在 HL 市价买 1.0（开仓加短腿）
    s.advance(600);
    let out = s.bbo(HL);
    let h = placed(&out);
    assert_eq!(h.len(), 1, "{out:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity, h[0].reduce_only), (HL, Side::Long, 1.0, false));
    assert!(matches!(h[0].order_type, OrderType::Market));

    // 补腿单在途，不重复补
    assert!(placed(&s.bbo(HL)).is_empty());
}

#[test]
fn reversal_closes_with_reduce_only_makers() {
    let mut s = Sim::new(-2.0, 2.0); // 币安空 HL 多
    s.funding(BN, 0.1);
    let out = s.funding(HL, 0.2); // HL 年化更高 → 反转
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    for x in &o {
        assert!(x.reduce_only);
        let want = if x.exchange == BN { Side::Long } else { Side::Short };
        assert_eq!(x.side, want);
        assert!((x.quantity - 1.998).abs() < 0.002);
    }
}

#[test]
fn startup_imbalance_halts_instead_of_trading() {
    // 启动时两腿差 1（可能有不归本策略的仓位）：不补、不开
    let mut s = Sim::without_bbo(-2.0, 3.0);
    s.bbo(BN);
    assert!(placed(&s.bbo(HL)).is_empty());
    s.funding(BN, 0.1);
    assert!(placed(&s.funding(HL, 0.9)).is_empty());
}

#[test]
fn hl_terminal_before_fill_does_not_double_hedge() {
    // HL maker 的「已成交」终态先到、成交回报晚到：窗口内不按旧持仓补
    let mut s = Sim::new(0.0, 0.0);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&bn, 1.998);
    s.confirm(&bn, OrderStatus::Filled);
    s.confirm(&hl, OrderStatus::Filled); // HL 也成交了，回报还没到
    let out = s.bbo(HL);
    assert!(placed(&out).is_empty(), "{out:?}");
    s.advance(100);
    let out = s.fill(&hl, 1.998); // 回报到达，两腿已平
    assert!(placed(&out).iter().all(|o| !matches!(o.order_type, OrderType::Market)), "{out:?}");
    s.advance(600);
    assert!(placed(&s.bbo(HL)).iter().all(|o| !matches!(o.order_type, OrderType::Market)));
}

#[test]
fn own_hedge_fill_arriving_right_after_placement_is_not_foreign() {
    // 补腿市价单发出后，下一个事件就是它的成交（中间没有别的事件）：不得停机
    let mut s = Sim::without_bbo(0.0, 0.0);
    s.bbo(BN);
    s.bbo(HL);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&bn, 1.998);
    s.confirm(&bn, OrderStatus::Filled);
    s.confirm(&hl, OrderStatus::Cancelled);
    s.advance(600);
    let h = placed(&s.bbo(HL));
    let hedge = h.iter().find(|o| matches!(o.order_type, OrderType::Market)).unwrap().clone();
    let a = s.fill(&hedge, 1.998); // 紧接着就是成交
    assert!(placed(&a).is_empty());
    let out = s.confirm(&hedge, OrderStatus::Filled);
    assert_eq!(placed(&out).len(), 2, "两腿平后继续挂开仓单，说明没停机：{out:?}");
}

#[test]
fn foreign_fill_halts() {
    let mut s = Sim::new(0.0, 0.0);
    let foreign = hft_engine_rs::domain::Order {
        id: String::new(),
        exchange: HL,
        symbol: SYM.into(),
        side: Side::Long,
        order_type: OrderType::Market,
        quantity: 1.0,
        reduce_only: false,
        client_order_id: "manual-ui-order".into(),
    };
    s.fill(&foreign, 1.0);
    s.advance(10_000);
    assert!(placed(&s.bbo(HL)).is_empty(), "外来成交后不得自动对冲");
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

#[test]
fn failed_cancel_is_retried() {
    let mut s = Sim::new(0.0, 0.0);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    for x in &o {
        s.confirm(x, OrderStatus::Pending);
    }
    // 资金费反转到持有以下 → 不挂，撤两张
    let out = s.funding(BN, 0.1);
    assert_eq!(cancels(&out).len(), 2, "{out:?}");
    // 撤单失败（没有任何事件），间隔内不重撤，过了间隔重撤
    assert!(cancels(&s.bbo(BN)).is_empty());
    s.advance(1_100);
    assert_eq!(cancels(&s.bbo(BN)).len(), 2);
}

#[test]
fn dust_imbalance_is_ignored() {
    // 差 0.1 × 100 = 10U < 最小单额 11U：不补
    let mut s = Sim::new(-2.0, 2.1);
    assert!(placed(&s.bbo(BN)).is_empty());
}

// ---------------------------------------------------------------- 多所（#673）

#[test]
fn three_exchanges_open_on_widest_pair_only() {
    let mut s = Sim::multi(&[(BN, 0.0), (HL, 0.0), (OKX, 0.0)]);
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
    let mut s = Sim::multi(&[(BN, 0.0), (HL, 0.0), (OKX, 0.0)]);
    s.funding(BN, 0.9);
    assert!(placed(&s.funding(HL, 0.0)).is_empty(), "OKX 年化未知时不知道哪对差最大");
}

#[test]
fn close_below_threshold_leaves_third_exchange_alone() {
    // 币安空 2、HL 多 1、OKX 多 1；BN−HL 收益 5% < 10% → 平 HL×BN，OKX×BN 收益 20% 不动
    let mut s = Sim::multi(&[(BN, -2.0), (HL, 1.0), (OKX, 1.0)]);
    s.funding(OKX, -0.1);
    assert!(placed(&s.funding(BN, 0.1)).is_empty(), "OKX×BN 收益 20% ≥ 门槛");
    // 平仓只看这对两所的年化：HL 一到就平
    let out = s.funding(HL, 0.05);
    let o = placed(&out);
    assert_eq!(o.len(), 2, "{out:?}");
    assert!(o.iter().all(|x| x.reduce_only && x.exchange != OKX), "{o:?}");
    let hl = o.iter().find(|x| x.exchange == HL).unwrap();
    assert_eq!(hl.side, Side::Short);
    assert!((hl.quantity - 1.0).abs() < 1e-9, "可平量 = min(HL 多 1, 币安空 2)：{}", hl.quantity);
}

#[test]
fn pair_earning_above_close_threshold_is_held() {
    // 币安空 HL 多，收益 15%：高于平仓门槛、低于开仓门槛 → 不动
    let mut s = Sim::new(-2.0, 2.0);
    s.funding(BN, 0.15);
    assert!(placed(&s.funding(HL, 0.0)).is_empty());
}

#[test]
fn three_exchanges_hedge_on_the_quoted_pair() {
    let mut s = Sim::multi(&[(BN, 0.0), (HL, 0.0), (OKX, 0.0)]);
    s.funding(BN, 0.2);
    s.funding(HL, 0.0);
    let o = placed(&s.funding(OKX, 0.9));
    let okx = o.iter().find(|o| o.exchange == OKX).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&okx, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&okx, 1.0);
    s.confirm(&okx, OrderStatus::Cancelled);
    s.confirm(&hl, OrderStatus::Cancelled);
    s.advance(600);
    let h = placed(&s.bbo(BN));
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity), (HL, Side::Long, 1.0), "OKX 卖成交 → 在 HL 买补，不碰币安");
    assert_eq!(okx.quantity, 1.0, "两腿同量：按 OKX 一手 1 个币向下取整");
    assert_eq!(hl.quantity, 1.0);
    assert!(matches!(h[0].order_type, OrderType::Market));
}

#[test]
fn both_close_legs_filling_needs_no_hedge() {
    // 平仓：HL 多 2 / 币安空 2，两边挂单各成交 1 → 仍对冲，不补腿
    let mut s = Sim::new(-2.0, 2.0);
    s.funding(BN, -0.1);
    let o = placed(&s.funding(HL, 0.0));
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    assert_eq!((hl.side, bn.side), (Side::Short, Side::Long));
    s.confirm(&hl, OrderStatus::Pending);
    s.confirm(&bn, OrderStatus::Pending);
    // 两边都成交 1（各平 1），持仓 BN −1 / HL 1，已对冲：不补
    s.fill(&hl, 1.0);
    s.fill(&bn, 1.0);
    s.confirm(&hl, OrderStatus::Cancelled);
    s.confirm(&bn, OrderStatus::Cancelled);
    s.advance(600);
    assert!(placed(&s.bbo(BN)).iter().all(|o| !matches!(o.order_type, OrderType::Market)));
}

#[test]
fn imbalance_below_one_lot_of_the_hedge_exchange_is_dust() {
    // OKX 一手 1 个币（≈100U）。HL 买单先成交 0.5（50U ≥ 最小单额，但 OKX 下不出 0.5）→ 不补、不停机；
    // HL 补满到 1.0 后在 OKX 补卖 1
    let mut s = Sim::multi(&[(HL, 0.0), (OKX, 0.0)]);
    s.funding(HL, 0.0);
    let o = placed(&s.funding(OKX, 0.9));
    let okx = o.iter().find(|o| o.exchange == OKX).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&okx, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::PartiallyFilled);
    s.fill(&hl, 0.5);
    s.advance(600);
    let out = s.bbo(HL);
    assert!(placed(&out).iter().all(|o| !matches!(o.order_type, OrderType::Market)), "零头不补：{out:?}");
    let c = cancels(&s.fill(&hl, 0.5));
    assert!(c.contains(&OKX), "差够一手：撤挂单准备补腿：{c:?}");
    s.confirm(&hl, OrderStatus::Filled);
    s.confirm(&okx, OrderStatus::Cancelled);
    s.advance(600);
    let h = placed(&s.bbo(HL));
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side, h[0].quantity), (OKX, Side::Short, 1.0));
}

#[test]
fn hedge_larger_than_target_position_is_not_reduce_only() {
    // 币安留着 0.05 个多仓零头（5U）。在「币安卖 / HL 买」开仓，HL 买单先成交 1.998：
    // 要在币安市价卖 1.998，远超 0.05 的多仓 → 不能设只减仓（否则被交易所截短或拒绝）
    let mut s = Sim::new(0.05, 0.0);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    let bn = o.iter().find(|o| o.exchange == BN).unwrap().clone();
    let hl = o.iter().find(|o| o.exchange == HL).unwrap().clone();
    s.confirm(&bn, OrderStatus::Pending);
    s.confirm(&hl, OrderStatus::Pending);
    s.fill(&hl, hl.quantity);
    s.confirm(&hl, OrderStatus::Filled);
    s.confirm(&bn, OrderStatus::Cancelled);
    s.advance(600);
    let h = placed(&s.bbo(BN));
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!((h[0].exchange, h[0].side), (BN, Side::Short));
    assert!(!h[0].reduce_only, "补腿量 {} 大于币安多仓 0.05", h[0].quantity);
}

#[test]
fn missing_bbo_on_unused_exchange_does_not_block_hedging() {
    // OKX 迟迟没盘口（如品种暂停）：币安×HL 照常开仓（OKX 年化也得已知才开 → 这里给 OKX 年化）
    let mut s = Sim::multi_without_bbo(&[(BN, 0.0), (HL, 0.0), (OKX, 0.0)]);
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
    let mut s = Sim::new(0.0, 0.0);
    s.funding(BN, 0.8);
    let o = placed(&s.funding(HL, 0.1));
    assert_eq!(o.len(), 2);
    for x in &o {
        s.confirm(x, OrderStatus::Pending);
    }
    // 测试配置最短挂单时间为 0：去留只看价格容差
    let out = s.bbo_depth(BN, 0.05);
    assert!(cancels(&out).is_empty() && placed(&out).is_empty(), "{out:?}");
    let out = s.bbo(BN);
    assert!(cancels(&out).is_empty() && placed(&out).is_empty(), "{out:?}");
}

#[test]
fn thin_book_does_not_place_new_makers() {
    let mut s = Sim::new(0.0, 0.0);
    s.bbo_depth(BN, 0.05);
    s.funding(BN, 0.8);
    assert!(placed(&s.funding(HL, 0.1)).is_empty());
}

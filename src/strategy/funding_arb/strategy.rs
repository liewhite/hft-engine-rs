//! 币安 × Hyperliquid 资金费套利策略（单币一个实例）。
//!
//! # 每个事件做一件事，按优先级
//!
//! 1. **两腿不平**（币安持仓 + HL 持仓 的价值 ≥ 最小单额）：先撤掉两所的挂单，等落后那所
//!    没有任何在途单后，对落后腿下市价单补齐。哪条算落后腿（用户规格）：开仓时加短腿
//!    （|持仓| 小的一所），平仓时砍长腿（|持仓| 大的一所）；方向未知（刚启动）按平仓处理，
//!    只减不加。补腿失败由框架记错误日志，本策略隔 `hedge_retry_ms` 重试并记 error。
//! 2. **两腿已平**：按过去 24h 年化决定开 / 平 / 持有（见 [`super::signal::decide`]），
//!    在两所各维护一张只做 maker 的挂单（费率高的一所卖、低的一所买；平仓反过来且只减仓）；
//!    挂单价偏离目标超过容差就撤了重挂。开仓前查两所各自的单币 / 账户杠杆，超限不挂。
//!
//! 一边成交即进入 1，另一边挂单被撤、市价补齐，回到 2 —— 这就是"一边成交撤另一腿、市价补腿"。
//!
//! # 停机（halt）
//!
//! 本策略把账户里该币的全部持仓当成自己的（框架持仓 = REST 基线 + 全部成交）。下面两种情况
//! 说明账户里有不归本策略的仓位或交易，自动"对冲"会下错单，故停止交易（撤自己的挂单、不开不补）
//! 并报 error，等人处理后重启：
//! - 启动时两腿已不平；
//! - 运行中收到不是本策略下的单的成交。
//!
//! # 残余窗口（如实声明）
//!
//! 补腿前要求两所最近一张本策略的单离开挂单列表已过 `fill_settle_ms`。若某所的成交回报晚于
//! 订单终态超过这个窗口，仍会多补一份；多补出来的差额在下一轮会按落后腿规则补回，代价是一次
//! 多余的吃单与短时敞口，不会累积。

use super::config::PairParams;
use super::signal::{decide, Direction, FundingAction, SettledFundingWindow};
use crate::domain::{Exchange, Order, OrderId, OrderType, Side, Symbol, TimeInForce, Timestamp, BBO};
use crate::exchange::SubscriptionKind;
use crate::messaging::{AccountData, IncomeEvent, MarketData, MarketEvent, PendingOrder};
use crate::strategy::{OutcomeEvent, Strategy, StrategyView, SymbolView};
use std::collections::{HashMap, HashSet, VecDeque};

const BN: Exchange = Exchange::Binance;
const HL: Exchange = Exchange::Hyperliquid;
const EXCHANGES: [Exchange; 2] = [BN, HL];
/// 记住多少个本策略订单号用于判别外来成交（成交回报最多晚到几秒，几百个足够）
const OWN_ID_MEMORY: usize = 1000;

/// 最近一次挂 maker 的意图，用来判定补腿时哪条是落后腿
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Open,
    Close,
}

/// 想在某所挂的一张 maker 单
#[derive(Debug, Clone, PartialEq)]
struct Quote {
    exchange: Exchange,
    side: Side,
    price: f64,
    qty: f64,
    reduce_only: bool,
}

pub struct FundingArbStrategy {
    p: PairParams,
    funding: HashMap<Exchange, SettledFundingWindow>,
    bbo_local_ts: HashMap<Exchange, Timestamp>,
    mode: Option<Mode>,
    /// 已发出撤单、尚未离开挂单列表的订单号 → 发出撤单的时刻。超过 `hedge_retry_ms` 仍在
    /// 就重撤：撤单 REST 失败时框架只返回错误、不发事件，由策略重试。
    cancelling: HashMap<OrderId, Timestamp>,
    /// 上一步看到的本策略挂单（client_order_id → 所），用来发现"离开挂单列表"
    prev_pending: HashMap<String, Exchange>,
    /// 各所最近一张本策略的单离开挂单列表的时刻（补腿结算窗口的起点）
    last_departure: HashMap<Exchange, Timestamp>,
    /// 最近见过的本策略订单号（判别外来成交）；只留最近的 OWN_ID_MEMORY 个
    own_ids: VecDeque<String>,
    own_id_set: HashSet<String>,
    /// 首次拿到两所盘口时做过启动检查没有
    startup_checked: bool,
    /// 停止交易的原因
    halted: Option<String>,
    last_hedge_ts: Option<Timestamp>,
    /// 本轮不平衡已发出的补腿单数；>0 时再发即是重试
    hedge_attempts: u32,
    last_action: Option<FundingAction>,
}

impl FundingArbStrategy {
    pub fn new(p: PairParams) -> Self {
        Self {
            p,
            funding: HashMap::new(),
            bbo_local_ts: HashMap::new(),
            mode: None,
            cancelling: HashMap::new(),
            prev_pending: HashMap::new(),
            last_departure: HashMap::new(),
            own_ids: VecDeque::new(),
            own_id_set: HashSet::new(),
            startup_checked: false,
            halted: None,
            last_hedge_ts: None,
            hedge_attempts: 0,
            last_action: None,
        }
    }

    fn symbol(&self) -> &Symbol {
        &self.p.symbol
    }

    fn bbo_fresh(&self, ex: Exchange, now: Timestamp) -> bool {
        self.bbo_local_ts
            .get(&ex)
            .is_some_and(|&t| now.saturating_sub(t) <= self.p.max_bbo_age_ms)
    }

    /// 跟踪本策略挂单的出现与离开
    fn track_pending(&mut self, sv: SymbolView<'_>, now: Timestamp) {
        let current: HashMap<String, Exchange> =
            sv.pending_orders().map(|p| (p.order.client_order_id.clone(), p.order.exchange)).collect();
        for (cid, ex) in &self.prev_pending {
            if !current.contains_key(cid) {
                self.last_departure.insert(*ex, now);
            }
        }
        for cid in current.keys() {
            if self.own_id_set.insert(cid.clone()) {
                self.own_ids.push_back(cid.clone());
                if self.own_ids.len() > OWN_ID_MEMORY {
                    if let Some(old) = self.own_ids.pop_front() {
                        self.own_id_set.remove(&old);
                    }
                }
            }
        }
        self.prev_pending = current;
        let live: HashSet<OrderId> = sv.pending_orders().map(|p| p.order.id.clone()).collect();
        self.cancelling.retain(|id, _| live.contains(id));
    }

    fn halt(&mut self, reason: String) {
        if self.halted.is_none() {
            tracing::error!(symbol = %self.symbol(), %reason, "停止交易：撤掉本策略挂单，不再开仓或补腿，需人工处理后重启");
            self.halted = Some(reason);
        }
    }

    fn step(&mut self, sv: SymbolView<'_>, view: StrategyView<'_>, now: Timestamp) -> Vec<OutcomeEvent> {
        self.track_pending(sv, now);
        if self.halted.is_some() {
            return self.maintain(sv, &[], now);
        }

        let (Some(bn_bbo), Some(hl_bbo)) = (sv.bbo(BN), sv.bbo(HL)) else {
            return Vec::new();
        };
        let px = (bn_bbo.mid_price() + hl_bbo.mid_price()) / 2.0;
        let bn = sv.position_size(BN);
        let hl = sv.position_size(HL);
        let net = bn + hl;

        if !self.startup_checked {
            self.startup_checked = true;
            if net.abs() * px >= self.p.min_order_notional {
                self.halt(format!("启动时两腿不平：币安 {bn}，HL {hl}（可能有不归本策略的仓位）"));
                return Vec::new();
            }
        }

        if net.abs() * px >= self.p.min_order_notional {
            return self.hedge(sv, net, bn, hl, now);
        }
        if self.hedge_attempts > 0 {
            tracing::info!(symbol = %self.symbol(), attempts = self.hedge_attempts, "两腿已平衡");
            self.hedge_attempts = 0;
        }

        // 在途的补腿单（市价）成交前不挂 maker：它的成交会改变两腿
        if sv.pending_orders().any(|p| matches!(p.order.order_type, OrderType::Market)) {
            return Vec::new();
        }

        let action = self.funding_action(bn, px, now);
        let fresh = self.bbo_fresh(BN, now) && self.bbo_fresh(HL, now);
        let quotes = match action {
            FundingAction::Open(dir) if fresh => self.open_quotes(dir, bn_bbo, hl_bbo, bn, hl, px, view),
            FundingAction::Close if fresh => self.close_quotes(bn_bbo, hl_bbo, bn, hl, px),
            _ => Vec::new(),
        };
        if !quotes.is_empty() {
            self.mode = Some(if matches!(action, FundingAction::Close) { Mode::Close } else { Mode::Open });
        }
        self.maintain(sv, &quotes, now)
    }

    /// 两所年化 → 动作；任一所数据未知时持有
    fn funding_action(&mut self, bn_pos: f64, px: f64, now: Timestamp) -> FundingAction {
        let apr = |ex| self.funding.get(&ex).and_then(|w| w.apr(now, self.p.max_funding_age_ms));
        let held = if bn_pos.abs() * px >= self.p.min_order_notional {
            Direction::from_binance_position(bn_pos)
        } else {
            None
        };
        let action = match (apr(BN), apr(HL)) {
            (Some(b), Some(h)) => {
                let a = decide(b, h, held, self.p.open_threshold_apr);
                if self.last_action != Some(a) {
                    tracing::info!(symbol = %self.symbol(), bn_apr = b, hl_apr = h, ?held, action = ?a, "资金费信号变化");
                }
                a
            }
            (b, h) => {
                if self.last_action != Some(FundingAction::Hold) {
                    tracing::warn!(symbol = %self.symbol(), bn_known = b.is_some(), hl_known = h.is_some(), "资金费数据未知或过期，不开不平");
                }
                FundingAction::Hold
            }
        };
        self.last_action = Some(action);
        action
    }

    /// 本次挂单金额上限：min(配置, 两所吃单侧第一档金额)。吃单侧 = 对面成交后另一所补腿
    /// 要吃的那一侧（卖腿的所补腿时在另一所买入，故看另一所卖一量，反之亦然）。
    fn cap_notional(&self, bn_side: Side, bn_bbo: &BBO, hl_bbo: &BBO) -> f64 {
        let take = |bbo: &BBO, side: Side| match side {
            Side::Long => bbo.ask_qty * bbo.ask_price,
            Side::Short => bbo.bid_qty * bbo.bid_price,
        };
        let hl_side = bn_side.opposite();
        self.p.order_notional.min(take(bn_bbo, bn_side)).min(take(hl_bbo, hl_side))
    }

    fn maker_price(&self, bbo: &BBO, side: Side) -> f64 {
        match side {
            Side::Short => bbo.ask_price * (1.0 + self.p.order_distance),
            Side::Long => bbo.bid_price * (1.0 - self.p.order_distance),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_quotes(
        &self,
        dir: Direction,
        bn_bbo: &BBO,
        hl_bbo: &BBO,
        bn: f64,
        hl: f64,
        px: f64,
        view: StrategyView<'_>,
    ) -> Vec<Quote> {
        let bn_side = match dir {
            Direction::ShortBinanceLongHl => Side::Short,
            Direction::LongBinanceShortHl => Side::Long,
        };
        let notional = self.cap_notional(bn_side, bn_bbo, hl_bbo);
        if notional < self.p.min_order_notional {
            return Vec::new();
        }
        for (ex, pos) in [(BN, bn), (HL, hl)] {
            if !self.leverage_allows(view, ex, pos.abs() * px, notional) {
                return Vec::new();
            }
        }
        let qty = notional / px;
        vec![
            Quote { exchange: BN, side: bn_side, price: self.maker_price(bn_bbo, bn_side), qty, reduce_only: false },
            Quote {
                exchange: HL,
                side: bn_side.opposite(),
                price: self.maker_price(hl_bbo, bn_side.opposite()),
                qty,
                reduce_only: false,
            },
        ]
    }

    fn close_quotes(&self, bn_bbo: &BBO, hl_bbo: &BBO, bn: f64, hl: f64, px: f64) -> Vec<Quote> {
        let held = bn.abs().min(hl.abs());
        if held * px < self.p.min_order_notional {
            return Vec::new();
        }
        let bn_side = if bn > 0.0 { Side::Short } else { Side::Long };
        let qty = (self.cap_notional(bn_side, bn_bbo, hl_bbo) / px).min(held);
        if qty * px < self.p.min_order_notional {
            return Vec::new();
        }
        vec![
            Quote { exchange: BN, side: bn_side, price: self.maker_price(bn_bbo, bn_side), qty, reduce_only: true },
            Quote {
                exchange: HL,
                side: bn_side.opposite(),
                price: self.maker_price(hl_bbo, bn_side.opposite()),
                qty,
                reduce_only: true,
            },
        ]
    }

    /// 加 `add` 美元持仓后，该所单币与账户杠杆是否仍在上限内。净值或持仓价值未知时不许开。
    fn leverage_allows(&self, view: StrategyView<'_>, ex: Exchange, symbol_notional: f64, add: f64) -> bool {
        let (Some(equity), Some(account)) = (view.equity(ex), view.account_notional(ex)) else {
            return false;
        };
        if equity <= 0.0 {
            return false;
        }
        let ok = (symbol_notional + add) / equity <= self.p.max_symbol_leverage
            && (account + add) / equity <= self.p.max_account_leverage;
        if !ok {
            tracing::debug!(symbol = %self.symbol(), %ex, equity, account, symbol_notional, add, "杠杆到上限，不再开仓");
        }
        ok
    }

    /// 让两所的挂单与 `quotes` 一致：不符的撤，缺的挂。
    fn maintain(&mut self, sv: SymbolView<'_>, quotes: &[Quote], now: Timestamp) -> Vec<OutcomeEvent> {
        let mut out = Vec::new();
        for ex in EXCHANGES {
            let want = quotes.iter().find(|q| q.exchange == ex);
            let pendings: Vec<&PendingOrder> = sv.pending_orders().filter(|p| p.order.exchange == ex).collect();
            if pendings.is_empty() {
                if let Some(q) = want {
                    out.push(self.place(q));
                }
                continue;
            }
            let mut kept = false;
            for p in pendings {
                if !kept && want.is_some_and(|q| self.matches(p, q, now)) {
                    kept = true;
                } else {
                    out.extend(self.cancel(p, now));
                }
            }
        }
        out
    }

    /// 挂着的单是否可以留着：方向与 reduce_only 一致，且价格在容差内或挂得还不够久
    fn matches(&self, p: &PendingOrder, q: &Quote, now: Timestamp) -> bool {
        let OrderType::Limit { price, .. } = p.order.order_type else {
            return false;
        };
        let young = now.saturating_sub(p.created_at) < self.p.min_requote_ms;
        p.order.side == q.side
            && p.order.reduce_only == q.reduce_only
            && (young || (price - q.price).abs() / q.price <= self.p.order_distance * self.p.requote_ratio)
    }

    fn place(&self, q: &Quote) -> OutcomeEvent {
        OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: q.exchange,
                symbol: self.symbol().clone(),
                side: q.side,
                order_type: OrderType::Limit { price: q.price, tif: TimeInForce::PostOnly },
                quantity: q.qty,
                reduce_only: q.reduce_only,
                client_order_id: String::new(),
            }],
            comment: format!(
                "funding_arb maker | {} {:?} qty={:.6} px={} reduce_only={}",
                q.exchange, q.side, q.qty, q.price, q.reduce_only
            ),
        }
    }

    /// 撤已确认的挂单（Created 还没有交易所订单号，撤不了，等确认）
    fn cancel(&mut self, p: &PendingOrder, now: Timestamp) -> Option<OutcomeEvent> {
        if !p.status.is_confirmed() {
            return None;
        }
        if let Some(&sent) = self.cancelling.get(&p.order.id) {
            if now.saturating_sub(sent) < self.p.hedge_retry_ms {
                return None;
            }
            tracing::warn!(symbol = %self.symbol(), exchange = %p.order.exchange, order_id = %p.order.id, "撤单未生效，重撤");
        }
        self.cancelling.insert(p.order.id.clone(), now);
        Some(OutcomeEvent::CancelOrder {
            exchange: p.order.exchange,
            symbol: self.symbol().clone(),
            order_id: p.order.id.clone(),
            client_order_id: p.order.client_order_id.clone(),
        })
    }

    /// 两腿不平：撤所有 maker，落后腿无在途单且过了重试间隔时市价补齐。
    fn hedge(&mut self, sv: SymbolView<'_>, net: f64, bn: f64, hl: f64, now: Timestamp) -> Vec<OutcomeEvent> {
        let mut out: Vec<OutcomeEvent> = sv
            .pending_orders()
            .filter(|p| matches!(p.order.order_type, OrderType::Limit { .. }))
            .cloned()
            .collect::<Vec<_>>()
            .iter()
            .filter_map(|p| self.cancel(p, now))
            .collect();

        // 开仓加短腿（|持仓| 小的所），平仓 / 未知砍长腿（|持仓| 大的所）
        let (target, target_pos) = match (self.mode, bn.abs() < hl.abs()) {
            (Some(Mode::Open), true) | (Some(Mode::Close) | None, false) => (BN, bn),
            _ => (HL, hl),
        };
        if sv.pending_orders().any(|p| p.order.exchange == target) {
            return out; // 等落后腿上的挂单撤完 / 在途补腿单出结果，免得重复成交
        }
        if self.last_hedge_ts.is_some_and(|t| now.saturating_sub(t) < self.p.hedge_retry_ms) {
            return out;
        }
        // 成交回报结算窗口：刚有单离开挂单列表时，它的成交可能还没到（见模块文档）
        if EXCHANGES
            .iter()
            .any(|ex| self.last_departure.get(ex).is_some_and(|&t| now.saturating_sub(t) < self.p.fill_settle_ms))
        {
            return out;
        }
        let side = if net > 0.0 { Side::Short } else { Side::Long };
        // 落后腿与净差同号 → 反向下单是在减它的仓
        let reduce_only = target_pos != 0.0 && target_pos.signum() == net.signum();
        if self.hedge_attempts > 0 {
            tracing::error!(
                symbol = %self.symbol(), %target, net, bn, hl, attempts = self.hedge_attempts,
                "补腿后两腿仍不平（补腿单失败或期间又有成交），重试"
            );
        }
        self.hedge_attempts += 1;
        self.last_hedge_ts = Some(now);
        out.push(OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: target,
                symbol: self.symbol().clone(),
                side,
                order_type: OrderType::Market,
                quantity: net.abs(),
                reduce_only,
                client_order_id: String::new(),
            }],
            comment: format!(
                "funding_arb hedge | {target} {side:?} qty={:.6} bn={bn} hl={hl} mode={:?}",
                net.abs(),
                self.mode
            ),
        });
        out
    }
}

impl Strategy for FundingArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        EXCHANGES
            .into_iter()
            .map(|ex| (ex, HashSet::from([SubscriptionKind::BBO { symbol: self.symbol().clone() }])))
            .collect()
    }

    /// 只作用于迟迟未获交易所确认（Created）的单；挂着的 maker 由本策略按价格偏离自行撤换
    fn order_timeout_ms(&self) -> u64 {
        60_000
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        let now = event.local_ts();
        // 分发层只投本币的事件，这里不再判 symbol（P4）
        match event {
            IncomeEvent::Market(MarketEvent { data: MarketData::BBO(bbo), .. }) => {
                self.bbo_local_ts.insert(bbo.exchange, now);
            }
            IncomeEvent::Market(MarketEvent { data: MarketData::Custom(c), .. }) => {
                if let Some(w) = c.get::<SettledFundingWindow>() {
                    self.funding.insert(w.exchange, w.clone());
                }
            }
            IncomeEvent::Account(a) => {
                if let AccountData::Fill(f) = &a.data {
                    let ours = f.client_order_id.as_ref().is_some_and(|c| self.own_id_set.contains(c));
                    if !ours {
                        self.halt(format!(
                            "收到非本策略订单的成交：{} {:?} {} cid={:?}",
                            f.exchange, f.side, f.size, f.client_order_id
                        ));
                    }
                }
            }
            _ => {}
        }
        let Some(sv) = view.symbol(self.symbol()) else {
            return Vec::new();
        };
        self.step(sv, view, now)
    }
}

//! 多所资金费套利策略（单币一个实例，覆盖该币上市的全部所，至少两所）。
//!
//! 每个币各所持仓相加应为 0（对冲）。操作永远发生在**一对所**之间（见 [`LegPair`]）：
//! 开仓在年化差最大的两所，平仓在收益率跌破门槛的那对相反仓位，第三所不动。
//!
//! # 每个事件做一件事，按优先级
//!
//! 1. **各所持仓之和不为 0**（价值 ≥ 最小单额，且补腿所下得出这个量）：先撤掉全部挂单，等补腿那所没有任何在途单后，
//!    在最近一次挂单的所对上市价补齐落后腿：多了就在该对的卖出所卖，少了就在买入所买。
//!    这正是用户规格「开仓加短腿、平仓砍长腿」：没成交的那条腿就是落后腿，补它即补齐这次操作；
//!    补过头（撤单期间另一边也成交）时反向补回的也是落后腿。补腿失败由框架记错误日志，本策略隔
//!    `hedge_retry_ms` 重试并记 error。
//! 2. **已对冲**：按各所过去 24h 年化决定开 / 平 / 持有（见 [`super::signal::decide`]），
//!    在选中的两所各维护一张只做 maker 的挂单（卖出所卖、买入所买；平仓只减仓）；
//!    挂单价偏离目标超过容差就撤了重挂。开仓前查两所各自的单币 / 账户杠杆，加仓超限不挂。
//!
//! 一边成交即进入 1，另一边挂单被撤、市价补齐，回到 2 —— 这就是"一边成交撤另一腿、市价补腿"。
//!
//! # 停机（halt）
//!
//! 本策略把账户里该币的全部持仓当成自己的（框架持仓 = REST 基线 + 全部成交）。下面几种情况
//! 说明账户里有不归本策略的仓位或交易，自动"对冲"会下错单，故停止交易（撤自己的挂单、不开不补）
//! 并报 error，等人处理后重启：
//! - 启动时各所持仓之和不为 0；
//! - 运行中收到不是本策略下的单的成交；
//! - 不平衡出现时还没挂过任何单（不知道该在哪对所补）。
//!
//! # 残余窗口（如实声明）
//!
//! 补腿前要求各所最近一张本策略的单离开挂单列表已过 `fill_settle_ms`。若某所的成交回报晚于
//! 订单终态超过这个窗口，仍会多补一份：之后会反向补回（结果多一来回的手续费），或在开仓时两腿
//! 各多出约一笔挂单金额（仍对冲，但不经杠杆检查）。每发生一次超出一笔，不会自动收回。
//!
//! # 零头（如实声明）
//!
//! 各所下单粒度不同（OKX 一手可值十几 U）。挂单量按两所粒度向下取整成两边都下得出的量；
//! 持仓差不足补腿所的一手（或最小下单量）时视为零头，不补 —— 补了也会被交易所拒。所以每个币
//! 可能留着不超过一手的单边敞口。
//!
//! 外来成交按订单号前缀判别（与框架同一判据）；HL 的前缀是格式要求，别的程序带 cloid 的单认不出。

use super::config::PairParams;
use super::signal::{decide, FundingAction, LegPair, SettledFundingWindow};
use crate::domain::{Exchange, Order, OrderId, OrderType, Side, Symbol, SymbolMeta, TimeInForce, Timestamp, BBO};
use crate::exchange::SubscriptionKind;
use crate::messaging::{AccountData, IncomeEvent, MarketData, MarketEvent, PendingOrder};
use crate::strategy::{OutcomeEvent, Strategy, StrategyView, SymbolView};
use std::collections::{HashMap, HashSet};

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
    /// 该币上市的所（固定顺序，决定同分时的取舍），至少两所
    exchanges: Vec<Exchange>,
    /// 各所合约规格：判断一个数量在该所下不下得出（精度与最小下单量的唯一出处）
    metas: HashMap<Exchange, SymbolMeta>,
    funding: HashMap<Exchange, SettledFundingWindow>,
    bbo_local_ts: HashMap<Exchange, Timestamp>,
    /// 最近一次挂 maker 的所对：不平衡时在这对所上补腿
    leg: Option<LegPair>,
    /// 已发出撤单、尚未离开挂单列表的订单号 → 发出撤单的时刻。超过 `hedge_retry_ms` 仍在
    /// 就重撤：撤单 REST 失败时框架只返回错误、不发事件，由策略重试。
    cancelling: HashMap<OrderId, Timestamp>,
    /// 上一步看到的本策略挂单（client_order_id → 所），用来发现"离开挂单列表"
    prev_pending: HashMap<String, Exchange>,
    /// 各所最近一张本策略的单离开挂单列表的时刻（补腿结算窗口的起点）
    last_departure: HashMap<Exchange, Timestamp>,
    /// 首次拿到全部所盘口时做过启动检查没有
    startup_checked: bool,
    /// 停止交易的原因
    halted: Option<String>,
    last_hedge_ts: Option<Timestamp>,
    /// 本轮不平衡已发出的补腿单数；>0 时再发即是重试
    hedge_attempts: u32,
    last_action: Option<FundingAction>,
    /// 上一次是否有所的资金费未知（只在变成未知时打一次 warn）
    funding_unknown: bool,
}

impl FundingArbStrategy {
    /// `metas`：该币在各所的合约规格，一所一份、至少两所，顺序即所的顺序（装配处按各所品种目录给出）
    pub fn new(p: PairParams, metas: Vec<SymbolMeta>) -> Self {
        let exchanges: Vec<Exchange> = metas.iter().map(|m| m.exchange).collect();
        let distinct: HashSet<_> = exchanges.iter().collect();
        assert!(
            exchanges.len() >= 2 && distinct.len() == exchanges.len() && metas.iter().all(|m| m.symbol == p.symbol),
            "{}: 资金费套利至少要两个不同的所、且规格属于本币，实际 {:?}",
            p.symbol,
            metas.iter().map(|m| (m.exchange, &m.symbol)).collect::<Vec<_>>()
        );
        Self {
            p,
            exchanges,
            metas: metas.into_iter().map(|m| (m.exchange, m)).collect(),
            funding: HashMap::new(),
            bbo_local_ts: HashMap::new(),
            leg: None,
            cancelling: HashMap::new(),
            prev_pending: HashMap::new(),
            last_departure: HashMap::new(),
            startup_checked: false,
            halted: None,
            last_hedge_ts: None,
            hedge_attempts: 0,
            last_action: None,
            funding_unknown: false,
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

        let mut bbos: HashMap<Exchange, &BBO> = HashMap::new();
        for &ex in &self.exchanges {
            match sv.bbo(ex) {
                Some(b) => {
                    bbos.insert(ex, b);
                }
                None => return Vec::new(),
            }
        }
        let px = bbos.values().map(|b| b.mid_price()).sum::<f64>() / bbos.len() as f64;
        let positions: Vec<(Exchange, f64)> = self.exchanges.iter().map(|&ex| (ex, sv.position_size(ex))).collect();
        let net: f64 = positions.iter().map(|(_, p)| p).sum();

        if !self.startup_checked {
            self.startup_checked = true;
            // 只看下得出单的差：哪所都下不出的零头是本策略自己留下的（见模块文档）
            if net.abs() * px >= self.p.min_order_notional && self.exchanges.iter().any(|&ex| self.tradable(ex, net.abs())) {
                self.halt(format!("启动时各所持仓之和不为 0：{positions:?}（可能有不归本策略的仓位）"));
                return Vec::new();
            }
        }

        if net.abs() * px >= self.p.min_order_notional {
            let Some(pair) = self.leg else {
                self.halt(format!("各所持仓之和不为 0 但本策略还没挂过单，不知道在哪对所补：{positions:?}"));
                return self.maintain(sv, &[], now);
            };
            if self.tradable(hedge_target(pair, net).0, net.abs()) {
                return self.hedge(sv, pair, net, &positions, now);
            }
        }
        if self.hedge_attempts > 0 {
            tracing::info!(symbol = %self.symbol(), attempts = self.hedge_attempts, "各所持仓已对冲");
            self.hedge_attempts = 0;
        }

        // 在途的补腿单（市价）成交前不挂 maker：它的成交会改变持仓
        if sv.pending_orders().any(|p| matches!(p.order.order_type, OrderType::Market)) {
            return Vec::new();
        }

        let action = self.funding_action(&positions, px, now);
        let fresh = |pair: LegPair| self.bbo_fresh(pair.sell, now) && self.bbo_fresh(pair.buy, now);
        let quotes = match action {
            FundingAction::Open(pair) if fresh(pair) => self.open_quotes(pair, &bbos, &positions, px, view),
            FundingAction::Close(pair) if fresh(pair) => self.close_quotes(pair, &bbos, &positions, px),
            _ => Vec::new(),
        };
        if let (Some(pair), false) = (action_pair(action), quotes.is_empty()) {
            self.leg = Some(pair);
        }
        self.maintain(sv, &quotes, now)
    }

    /// 各所年化 → 动作
    fn funding_action(&mut self, positions: &[(Exchange, f64)], px: f64, now: Timestamp) -> FundingAction {
        let aprs: Vec<(Exchange, Option<f64>)> = self
            .exchanges
            .iter()
            .map(|&ex| (ex, self.funding.get(&ex).and_then(|w| w.apr(now, self.p.max_funding_age_ms))))
            .collect();
        let held: Vec<(Exchange, f64)> = positions
            .iter()
            .copied()
            .filter(|(_, p)| p.abs() * px >= self.p.min_order_notional)
            .collect();

        let unknown: Vec<Exchange> = aprs.iter().filter(|(_, a)| a.is_none()).map(|(e, _)| *e).collect();
        if unknown.is_empty() {
            self.funding_unknown = false;
        } else if !self.funding_unknown {
            self.funding_unknown = true;
            tracing::warn!(symbol = %self.symbol(), ?unknown, "资金费数据未知或过期：不开仓，只平两所数据都在的仓位对");
        }

        let action = decide(&aprs, &held, self.p.open_threshold_apr, self.p.close_threshold_apr);
        if self.last_action != Some(action) {
            tracing::info!(symbol = %self.symbol(), ?aprs, ?held, ?action, "资金费信号变化");
        }
        self.last_action = Some(action);
        action
    }

    /// 本次挂单金额上限：min(配置, 两所吃单侧第一档金额)。吃单侧 = 一边成交后另一所补腿
    /// 要吃的那一侧：卖出所的单若没成交、要在它那里市价卖，看它的买一量；买入所同理看卖一量。
    fn cap_notional(&self, pair: LegPair, bbos: &HashMap<Exchange, &BBO>) -> f64 {
        let sell = bbos[&pair.sell];
        let buy = bbos[&pair.buy];
        self.p
            .order_notional
            .min(sell.bid_qty * sell.bid_price)
            .min(buy.ask_qty * buy.ask_price)
    }

    fn maker_price(&self, bbo: &BBO, side: Side) -> f64 {
        match side {
            Side::Short => bbo.ask_price * (1.0 + self.p.order_distance),
            Side::Long => bbo.bid_price * (1.0 - self.p.order_distance),
        }
    }

    /// 这个币数量在该所能不能下出去（取整后非 0 且不低于最小下单量）
    fn tradable(&self, ex: Exchange, qty: f64) -> bool {
        self.metas[&ex].checked_exchange_qty(qty).is_ok()
    }

    /// 两所挂同样的量：按两所精度依次向下取整（各所步长都是 10 的幂次，取整后同为两所步长的整数倍）；
    /// 任一所下不出则不挂。
    fn pair_quotes(&self, pair: LegPair, bbos: &HashMap<Exchange, &BBO>, qty: f64, reduce_only: bool) -> Vec<Quote> {
        let qty = self.metas[&pair.buy].round_coin_size_down(self.metas[&pair.sell].round_coin_size_down(qty));
        if !(self.tradable(pair.sell, qty) && self.tradable(pair.buy, qty)) {
            return Vec::new();
        }
        [(pair.sell, Side::Short), (pair.buy, Side::Long)]
            .into_iter()
            .map(|(ex, side)| Quote { exchange: ex, side, price: self.maker_price(bbos[&ex], side), qty, reduce_only })
            .collect()
    }

    fn open_quotes(
        &self,
        pair: LegPair,
        bbos: &HashMap<Exchange, &BBO>,
        positions: &[(Exchange, f64)],
        px: f64,
        view: StrategyView<'_>,
    ) -> Vec<Quote> {
        let notional = self.cap_notional(pair, bbos);
        if notional < self.p.min_order_notional {
            return Vec::new();
        }
        let qty = notional / px;
        let pos = |ex: Exchange| positions.iter().find(|(e, _)| *e == ex).map_or(0.0, |(_, p)| *p);
        for (ex, delta) in [(pair.sell, -qty), (pair.buy, qty)] {
            if !self.leverage_allows(view, ex, pos(ex), delta, px) {
                return Vec::new();
            }
        }
        // 不设只减仓：开仓可能顺带削减该所已有的腿（用户允许），并可能越过 0 变成反向仓
        self.pair_quotes(pair, bbos, qty, false)
    }

    fn close_quotes(&self, pair: LegPair, bbos: &HashMap<Exchange, &BBO>, positions: &[(Exchange, f64)], px: f64) -> Vec<Quote> {
        let pos = |ex: Exchange| positions.iter().find(|(e, _)| *e == ex).map_or(0.0, |(_, p)| *p);
        // 卖出所持多、买入所持空（decide 保证），可平的量 = 两者绝对值较小的一个
        let held = pos(pair.sell).max(0.0).min((-pos(pair.buy)).max(0.0));
        let qty = (self.cap_notional(pair, bbos) / px).min(held);
        if qty * px < self.p.min_order_notional {
            return Vec::new();
        }
        self.pair_quotes(pair, bbos, qty, true)
    }

    /// 在该所把持仓从 `pos` 改到 `pos + delta` 后，单币与账户杠杆是否仍在上限内。
    /// 持仓绝对值不增（削减已有腿）时总是允许 —— 超限只停止开仓，不拦减仓。
    /// 净值或持仓总价值未知时不许加仓。
    fn leverage_allows(&self, view: StrategyView<'_>, ex: Exchange, pos: f64, delta: f64, px: f64) -> bool {
        let before = pos.abs() * px;
        let after = (pos + delta).abs() * px;
        if after <= before {
            return true;
        }
        let (Some(equity), Some(account)) = (view.equity(ex), view.account_notional(ex)) else {
            return false;
        };
        if equity <= 0.0 {
            return false;
        }
        let account_after = account - before + after;
        let ok = after / equity <= self.p.max_symbol_leverage && account_after / equity <= self.p.max_account_leverage;
        if !ok {
            tracing::debug!(symbol = %self.symbol(), %ex, equity, account, before, after, "杠杆到上限，不再开仓");
        }
        ok
    }

    /// 让各所的挂单与 `quotes` 一致：不符的撤，缺的挂。
    fn maintain(&mut self, sv: SymbolView<'_>, quotes: &[Quote], now: Timestamp) -> Vec<OutcomeEvent> {
        let mut out = Vec::new();
        for ex in self.exchanges.clone() {
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

    /// 持仓之和不为 0：撤所有 maker，补腿所无在途单且过了重试间隔时市价补齐。
    fn hedge(&mut self, sv: SymbolView<'_>, pair: LegPair, net: f64, positions: &[(Exchange, f64)], now: Timestamp) -> Vec<OutcomeEvent> {
        let mut out: Vec<OutcomeEvent> = sv
            .pending_orders()
            .filter(|p| matches!(p.order.order_type, OrderType::Limit { .. }))
            .cloned()
            .collect::<Vec<_>>()
            .iter()
            .filter_map(|p| self.cancel(p, now))
            .collect();

        let (target, side) = hedge_target(pair, net);
        if sv.pending_orders().any(|p| p.order.exchange == target) {
            return out; // 等补腿所上的挂单撤完 / 在途补腿单出结果，免得重复成交
        }
        if self.last_hedge_ts.is_some_and(|t| now.saturating_sub(t) < self.p.hedge_retry_ms) {
            return out;
        }
        // 成交回报结算窗口：刚有单离开挂单列表时，它的成交可能还没到（见模块文档）
        if self
            .exchanges
            .iter()
            .any(|ex| self.last_departure.get(ex).is_some_and(|&t| now.saturating_sub(t) < self.p.fill_settle_ms))
        {
            return out;
        }
        let target_pos = positions.iter().find(|(e, _)| *e == target).map_or(0.0, |(_, p)| *p);
        // 补腿所持仓与净差同号 → 反向下单是在减它的仓
        let reduce_only = target_pos != 0.0 && target_pos.signum() == net.signum();
        if self.hedge_attempts > 0 {
            tracing::error!(
                symbol = %self.symbol(), %target, net, ?positions, attempts = self.hedge_attempts,
                "补腿后仍不平（补腿单失败或期间又有成交），重试"
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
            comment: format!("funding_arb hedge | {target} {side:?} qty={:.6} positions={positions:?} pair={pair:?}", net.abs()),
        });
        out
    }
}

/// 多了在卖出所卖、少了在买入所买：补的就是这次操作里没成交的落后腿
fn hedge_target(pair: LegPair, net: f64) -> (Exchange, Side) {
    if net > 0.0 {
        (pair.sell, Side::Short)
    } else {
        (pair.buy, Side::Long)
    }
}

fn action_pair(action: FundingAction) -> Option<LegPair> {
    match action {
        FundingAction::Open(p) | FundingAction::Close(p) => Some(p),
        FundingAction::Hold => None,
    }
}

impl Strategy for FundingArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        self.exchanges
            .iter()
            .map(|&ex| (ex, HashSet::from([SubscriptionKind::BBO { symbol: self.symbol().clone() }])))
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
                    // 归属判据与框架同一份（订单号前缀）。盖不住本引擎另一个进程在同账户同币上交易；
                    // 本策略一个进程覆盖全部币，不会出现。
                    let ours = f.client_order_id.as_ref().is_some_and(|c| f.exchange.owns_cli_order_id(c));
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

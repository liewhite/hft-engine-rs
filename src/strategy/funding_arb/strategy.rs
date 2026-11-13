//! 多所资金费套利策略（单币一个实例，覆盖该币上市的全部所，至少两所）。
//!
//! 每个币各所持仓相加应为 0（对冲）。操作永远发生在**一对所**之间（见 [`LegPair`]）：
//! 开仓在年化差最大的两所，平仓在收益率跌破门槛的那对相反仓位，第三所不动。
//!
//! # 每个事件做一件事，按优先级
//!
//! 0. **有单已结束、成交还在路上**（框架的订单跟踪表里有不再开着的单，见 `PendingOrder`）：持仓还会变，
//!    只撤开着的 maker，等成交到齐。
//! 1. **各所持仓之和不为 0**（价值 ≥ 最小单额，且补腿所下得出这个量）：先撤掉全部挂单，等本币所有单都结算完
//!    （撤单确认、成交入账）后，在最近一次挂单的所对上市价补齐落后腿：多了就在该对的卖出所卖，少了就在买入所买。
//!    这正是用户规格「开仓加短腿、平仓砍长腿」：没成交的那条腿就是落后腿，补它即补齐这次操作；
//!    补过头（撤单期间另一边也成交）时反向补回的也是落后腿。补腿被交易所拒绝时框架回报失败，本策略隔
//!    `hedge_retry_ms` 重试并记 error；补腿结果不确定（超时、5xx 等）时框架直接停机（#692）。
//! 2. **已对冲**：按各所过去 24h 年化决定开 / 平 / 持有（见 [`super::signal::decide`]），
//!    在选中的两所各维护一张只做 maker 的挂单（卖出所卖、买入所买；平仓只减仓）；
//!    挂单价偏离目标超过容差就撤了重挂。开仓前查两所各自的单币 / 账户杠杆，加仓超限不挂。
//!
//! 一边成交即进入 1，另一边挂单被撤、市价补齐，回到 2 —— 这就是"一边成交撤另一腿、市价补腿"。
//!
//! # 停机（halt）
//!
//! 账户由本策略全权托管（用户定），不检测外来交易（#760 用户定）：账户里该币的全部持仓都当成
//! 自己的（框架持仓 = REST 基线 + 全部成交），别的单的成交按普通不平衡补；品种目录外的成交
//! （框架以 `UnmappedFill` 上报）不理会。
//!
//! 只在不知道该在哪对所补时停止交易（撤自己的挂单、不开不补）并报 error，等人处理后重启：
//! - 启动时各所持仓之和不为 0；
//! - 不平衡出现时还没挂过任何单。
//!
//! # 成交与订单回报的先后
//!
//! 一张单要等交易所说它结束、且它的成交按交易所说的累计量全部入账，才离开框架的跟踪表（#758）。
//! 所以「挂单没了」时持仓就是最终的：不会在成交还没到时以为两腿已平、在同所再挂一张，也不会多补一份。
//! 迟迟等不到的（一分钟）框架直接停机。
//!
//! # 零头（如实声明）
//!
//! 各所下单粒度不同（OKX 一手可值十几 U）。挂单量按两所粒度向下取整成两边都下得出的量；
//! 持仓差不足补腿所的一手、最小下单量或最小下单金额（配置下限与交易所下限取大）时视为零头，
//! 不补 —— 补了也会被交易所拒。所以每个币可能留着不超过一笔最小单的单边敞口。判据见 `is_dust`。
//!
//! 所对切换的那一步，旧所对的挂单撤销在途时新所对已挂单；新旧同方向的单都成交时，补腿量是两笔、
//! 不经杠杆检查，最多超出一笔挂单金额。

use super::config::PairParams;
use super::signal::{decide, FundingAction, LegPair, SettledFundingWindow};
use crate::domain::{Exchange, Order, OrderStatus, OrderType, Side, Symbol, SymbolMeta, TimeInForce, Timestamp, BBO};
use crate::exchange::SubscriptionKind;
use crate::messaging::{
    AccountData, CancelState, IncomeEvent, MarketData, MarketEvent, PendingOrder, RequestOutcome, SymbolAccountData,
};
use crate::observability::Gauge;
use crate::strategy::{OutcomeEvent, Strategy, StrategyView, SymbolView};
use std::collections::{HashMap, HashSet};

/// 想在某所挂的一张 maker 单
#[derive(Debug, Clone, PartialEq)]
struct Quote {
    exchange: Exchange,
    side: Side,
    price: f64,
    /// `None` = 这一刻定不出量（对手盘第一档太薄、或凑不齐两所都下得出的一手）：不新挂，
    /// 但已挂着的同向单按价格容差照常保留。盘口厚度一跳一跳地变，否则会挂了马上撤、撤了又挂
    /// （实盘 11-12 首轮 OKX INIT 一秒撤挂数次），白耗下单额度
    qty: Option<f64>,
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
    /// 收到第一条事件的时刻：某所从没来过盘口时，盘口年龄指标从这里算起
    first_event_at: Option<Timestamp>,
    /// 最近一次挂 maker 的所对：不平衡时在这对所上补腿
    leg: Option<LegPair>,
    /// 首次拿到全部所盘口时做过启动检查没有
    startup_checked: bool,
    /// 停止交易的原因
    halted: Option<String>,
    last_hedge_ts: Option<Timestamp>,
    /// 本轮不平衡已发出的补腿单数；>0 时再发即是重试
    hedge_attempts: u32,
    last_action: Option<FundingAction>,
    /// 已报过「挂不出平仓单」的所对（只报一次）
    close_stuck: Option<LegPair>,
    /// 上一次是否有所的资金费未知（只在变成未知时打一次 warn）
    funding_unknown: bool,
    /// 各所下单被拒后的退避（见 [`RejectBackoff`]）
    reject_backoff: HashMap<Exchange, RejectBackoff>,
}

/// 某所连续被拒单时的退避：第一次拒后等 `hedge_retry_ms` 再挂新的 maker，之后每连续拒一次等待
/// 翻倍，封顶 [`MAX_REJECT_BACKOFF_MS`]；该所有一张单被交易所接下（挂上或成交）就清零。
///
/// 不退避时，被拒的单离开挂单列表后下一个盘口事件就重挂：持续性拒单（保证金不足、币安
/// -2027 杠杆档位、限频）会按 REST 往返的速度刷单，招来币安 418 封 IP、耗光 HL 每日操作额度。
/// post-only 会吃单被拒这类偶发拒单只等一个间隔，对资费套利无关紧要，故不按拒单原因区分。
/// 口径是该所本策略的任何单：市价补腿被拒也会让该所的 maker 退避，补腿成交也会清零；
/// 但退避只拦新挂 maker，不拦补腿（补腿另有 `hedge_retry_ms` 的重试间隔）。同一张单的拒单若
/// 经 REST 与私有流各到一次会翻倍两次，有封顶，不处理。
#[derive(Debug, Clone, Copy)]
struct RejectBackoff {
    /// 这之前不在该所挂新的 maker
    until: Timestamp,
    /// 本次等待时长（下次再拒就翻倍）
    delay_ms: u64,
}

/// 拒单退避的上限：持续拒单时每所每币最多 5 分钟试一次
const MAX_REJECT_BACKOFF_MS: u64 = 5 * 60_000;

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
            first_event_at: None,
            leg: None,
            startup_checked: false,
            halted: None,
            last_hedge_ts: None,
            hedge_attempts: 0,
            last_action: None,
            close_stuck: None,
            funding_unknown: false,
            reject_backoff: HashMap::new(),
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

    fn pair_fresh(&self, pair: LegPair, now: Timestamp) -> bool {
        self.bbo_fresh(pair.sell, now) && self.bbo_fresh(pair.buy, now)
    }

    fn halt(&mut self, reason: String) {
        if self.halted.is_none() {
            tracing::error!(symbol = %self.symbol(), %reason, "停止交易：撤掉本策略挂单，不再开仓或补腿，需人工处理后重启");
            self.halted = Some(reason);
        }
    }

    fn step(&mut self, sv: SymbolView<'_>, view: StrategyView<'_>, now: Timestamp) -> Vec<OutcomeEvent> {
        if self.halted.is_some() {
            return self.maintain(sv, &[], now);
        }
        // 有单已结束、成交还在路上：持仓还会变，按现在的持仓开平或补腿都会错。那笔成交一到就是不平衡，
        // 开着的 maker 本来也要撤，先撤
        if sv.pending_orders().any(|p| !p.is_open()) {
            return self.cancel_makers(sv, now);
        }

        // 不要求每所都有盘口：某所品种暂停、迟迟没盘口时，其余所的补腿和停机检查照常；
        // 挂单只在用到的两所盘口新鲜时才挂（见下方 fresh）
        let bbos: HashMap<Exchange, &BBO> =
            self.exchanges.iter().filter_map(|&ex| sv.bbo(ex).map(|b| (ex, b))).collect();
        if bbos.is_empty() {
            return Vec::new();
        }
        // 参考价（币↔金额换算：挂单量、零头判定；杠杆闸门用各所自己的价）只用新鲜盘口；都不新鲜时
        // 退回全部盘口 —— 此时只用于零头判定和补腿，挂单另要求所对两所盘口新鲜，不会用到它
        let fresh_mids: Vec<f64> =
            bbos.iter().filter(|(ex, _)| self.bbo_fresh(**ex, now)).map(|(_, b)| b.mid_price()).collect();
        let mids: Vec<f64> = if fresh_mids.is_empty() { bbos.values().map(|b| b.mid_price()).collect() } else { fresh_mids };
        let px = mids.iter().sum::<f64>() / mids.len() as f64;
        let positions: Vec<(Exchange, f64)> = self.exchanges.iter().map(|&ex| (ex, sv.position_size(ex))).collect();
        let net: f64 = positions.iter().map(|(_, p)| p).sum();

        let startup = !self.startup_checked;
        self.startup_checked = true;
        if !self.is_dust(net, px) {
            match (self.leg, startup) {
                (Some(pair), false) => return self.hedge(sv, pair, net, &positions, now),
                (_, true) => self.halt(format!("启动时各所持仓之和不为 0：{positions:?}（可能有不归本策略的仓位）")),
                (None, false) => {
                    self.halt(format!("各所持仓之和不为 0 但本策略还没挂过单，不知道在哪对所补：{positions:?}"))
                }
            }
            return self.maintain(sv, &[], now);
        }
        if self.hedge_attempts > 0 {
            let floor = self.leg.map_or(self.p.min_order_notional, |pair| self.min_notional(hedge_target(pair, net).0));
            if net.abs() * px >= floor {
                tracing::warn!(symbol = %self.symbol(), net, ?positions, "补腿后剩下补腿所下不出的零头，不再补");
            } else {
                tracing::info!(symbol = %self.symbol(), attempts = self.hedge_attempts, "各所持仓已对冲");
            }
            self.hedge_attempts = 0;
        }

        // 在途的补腿单（市价）成交前不挂 maker：它的成交会改变持仓
        if sv.pending_orders().any(|p| matches!(p.order.order_type, OrderType::Market)) {
            return Vec::new();
        }

        let action = self.funding_action(&positions, px, now);
        let quotes = match action {
            FundingAction::Open(pair) if self.pair_fresh(pair, now) => self.open_quotes(pair, &bbos, &positions, px, view),
            FundingAction::Close(pair) if self.pair_fresh(pair, now) => self.close_quotes(pair, &bbos, &positions, px),
            _ => Vec::new(),
        };
        let sized = quotes.iter().any(|q| q.qty.is_some());
        if let (Some(pair), true) = (action_pair(action), sized) {
            self.leg = Some(pair);
        }
        match action {
            // 平仓信号在、盘口也新鲜，却挂不出单（量不足最小单或凑不齐两所都下得出的一手）：
            // 仓位会一直拿着，要人看。每个所对只报一次
            FundingAction::Close(pair) if !sized && self.pair_fresh(pair, now) => {
                if self.close_stuck != Some(pair) {
                    tracing::warn!(symbol = %self.symbol(), ?pair, ?positions, "该平仓但挂不出平仓单（量太小或下单粒度凑不齐），需人工处理");
                    self.close_stuck = Some(pair);
                }
            }
            _ if sized => self.close_stuck = None,
            _ => {}
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
            .filter(|(ex, p)| p.abs() * px >= self.min_notional(*ex))
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

    /// 持仓差是不是零头（不补、不停机），全策略只此一处判据：补不出去就是零头（见 [`Self::tradable`]）。
    /// 有所对时看补腿所下不下得出；还没有所对（启动时）时，只要有任一所下不出就算零头
    /// —— 本策略留下的零头不超过当时补腿所的一笔最小单，而那一所事后未知，按最粗的一所放宽，
    /// 免得每次重启都因自己的零头停机。代价：启动时不足最粗一所最小单的不平会被放过。
    fn is_dust(&self, net: f64, px: f64) -> bool {
        match self.leg {
            Some(pair) => !self.tradable(hedge_target(pair, net).0, net.abs(), px),
            None => !self.exchanges.iter().all(|&ex| self.tradable(ex, net.abs(), px)),
        }
    }

    /// 该所实际用的最小下单金额：配置下限与交易所下限取大（用户 11-12 定）
    fn min_notional(&self, ex: Exchange) -> f64 {
        self.p.min_order_notional.max(self.metas[&ex].min_notional)
    }

    /// 一对所都下得出的最小金额
    fn pair_min_notional(&self, pair: LegPair) -> f64 {
        self.min_notional(pair.sell).max(self.min_notional(pair.buy))
    }

    /// 这个币数量按参考价 `px` 在该所能不能下出去：取整后非 0、不低于最小下单量，
    /// 且取整后的金额不低于该所最小下单金额
    fn tradable(&self, ex: Exchange, qty: f64, px: f64) -> bool {
        let meta = &self.metas[&ex];
        meta.checked_exchange_qty(qty).is_ok() && meta.round_coin_size_down(qty) * px >= self.min_notional(ex)
    }

    /// 两所挂同样的量：按两所精度依次向下取整（各所步长都是 10 的幂次，取整后同为两所步长的整数倍）；
    /// 任一所下不出则量为 `None`（不新挂，见 [`Quote::qty`]）。
    fn pair_quotes(
        &self,
        pair: LegPair,
        bbos: &HashMap<Exchange, &BBO>,
        qty: f64,
        px: f64,
        reduce_only: bool,
    ) -> Vec<Quote> {
        let qty = self.metas[&pair.buy].round_coin_size_down(self.metas[&pair.sell].round_coin_size_down(qty));
        let qty = (self.tradable(pair.sell, qty, px) && self.tradable(pair.buy, qty, px)).then_some(qty);
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
        if notional < self.pair_min_notional(pair) {
            return self.pair_quotes(pair, bbos, 0.0, px, false);
        }
        let qty = notional / px;
        let pos = |ex: Exchange| positions.iter().find(|(e, _)| *e == ex).map_or(0.0, |(_, p)| *p);
        for (ex, delta) in [(pair.sell, -qty), (pair.buy, qty)] {
            // 超限和「定不出量」同一处理：不新挂，已挂的按价格容差留着。持仓卡在上限边上时闸门随盘口
            // 小跳来回翻，超限就撤会挂了即撤、白耗 HL 操作额度（#769）。已挂的单是在放行时按成交后不超限
            // 定的量，留着成交只会因之后的价格、净值波动略超一点
            if !self.leverage_allows(view, ex, pos(ex), delta, bbos[&ex].mid_price()) {
                return self.pair_quotes(pair, bbos, 0.0, px, false);
            }
        }
        // 不设只减仓：开仓可能顺带削减该所已有的腿（用户允许），并可能越过 0 变成反向仓
        self.pair_quotes(pair, bbos, qty, px, false)
    }

    fn close_quotes(&self, pair: LegPair, bbos: &HashMap<Exchange, &BBO>, positions: &[(Exchange, f64)], px: f64) -> Vec<Quote> {
        let pos = |ex: Exchange| positions.iter().find(|(e, _)| *e == ex).map_or(0.0, |(_, p)| *p);
        // 卖出所持多、买入所持空（decide 保证），可平的量 = 两者绝对值较小的一个
        let held = pos(pair.sell).max(0.0).min((-pos(pair.buy)).max(0.0));
        let qty = (self.cap_notional(pair, bbos) / px).min(held);
        if qty * px < self.pair_min_notional(pair) {
            return self.pair_quotes(pair, bbos, 0.0, px, true);
        }
        self.pair_quotes(pair, bbos, qty, px, true)
    }

    /// 在该所把持仓从 `pos` 改到 `pos + delta` 后，单币与账户杠杆是否仍在上限内。
    /// `px` 是该所自己的盘口中间价：杠杆每所各算，用该所的价才和交易所、账本里别的币的口径一致
    /// （几所均价会把价高的所低估，#769）。
    /// 持仓绝对值不增（削减已有腿）时总是允许 —— 超限只停止开仓，不拦减仓。
    /// 净值或持仓总价值未知时不许加仓。
    ///
    /// 账户持仓总价值 = 本地持仓账本里该所**别的币**的持仓价值之和 + 本币开仓后的价值（#759，用户定：
    /// 本地所有持仓价值相加）。本币用自己这一刻的持仓，不用账本那份（可能还没含刚到的成交）；
    /// 别的币的成交由账本在成交后立刻发出，不等交易所轮询。只算配置里的币，账户里别的仓位不计。
    fn leverage_allows(&self, view: StrategyView<'_>, ex: Exchange, pos: f64, delta: f64, px: f64) -> bool {
        let before = pos.abs() * px;
        let after = (pos + delta).abs() * px;
        if after <= before {
            return true;
        }
        let (Some(equity), Some(values)) = (view.equity(ex), view.position_values(ex)) else {
            return false;
        };
        if equity <= 0.0 {
            return false;
        }
        let others: f64 = values.iter().filter(|(s, _)| *s != self.symbol()).map(|(_, v)| v).sum();
        let account_after = others + after;
        let ok = after / equity <= self.p.max_symbol_leverage && account_after / equity <= self.p.max_account_leverage;
        if !ok {
            tracing::debug!(symbol = %self.symbol(), %ex, equity, others, before, after, "杠杆到上限，不再开仓");
        }
        ok
    }

    /// 记下本策略一张单的结局：被拒（交易所拒，或本地下单请求被拒）则加长该所的退避，被接下则清零
    fn note_order_outcome(&mut self, data: &SymbolAccountData, now: Timestamp) {
        let (ex, rejected) = match data {
            SymbolAccountData::OrderUpdate(u) if u.client_order_id.as_ref().is_some_and(|c| u.exchange.owns_cli_order_id(c)) => {
                match &u.status {
                    OrderStatus::Rejected { reason } => (u.exchange, Some(reason)),
                    OrderStatus::Pending | OrderStatus::PartiallyFilled | OrderStatus::Filled => (u.exchange, None),
                    OrderStatus::Created | OrderStatus::Cancelled => return,
                }
            }
            SymbolAccountData::OrderRequest(r) => match &r.outcome {
                RequestOutcome::PlaceRejected { reason } => (r.exchange, Some(reason)),
                RequestOutcome::Placed { .. } => (r.exchange, None),
                RequestOutcome::CancelFailed { .. } => return,
            },
            _ => return,
        };
        match rejected {
            Some(reason) => {
                let delay_ms = self
                    .reject_backoff
                    .get(&ex)
                    .map_or(self.p.hedge_retry_ms, |b| (b.delay_ms * 2).min(MAX_REJECT_BACKOFF_MS));
                tracing::warn!(symbol = %self.symbol(), %ex, %reason, delay_ms, "下单被拒，该所暂停挂新单");
                self.reject_backoff.insert(ex, RejectBackoff { until: now + delay_ms, delay_ms });
            }
            None => {
                self.reject_backoff.remove(&ex);
            }
        }
    }

    fn backing_off(&self, ex: Exchange, now: Timestamp) -> bool {
        self.reject_backoff.get(&ex).is_some_and(|b| now < b.until)
    }

    /// 让各所的挂单与 `quotes` 一致：不符的撤，缺的挂（拒单退避中的所不挂）。
    fn maintain(&mut self, sv: SymbolView<'_>, quotes: &[Quote], now: Timestamp) -> Vec<OutcomeEvent> {
        let mut out = Vec::new();
        for ex in self.exchanges.clone() {
            let want = quotes.iter().find(|q| q.exchange == ex);
            let pendings: Vec<&PendingOrder> = sv.pending_orders().filter(|p| p.order.exchange == ex).collect();
            if pendings.is_empty() {
                if let Some((q, qty)) = want.and_then(|q| q.qty.map(|n| (q, n))) {
                    if !self.backing_off(ex, now) {
                        out.push(self.place(q, qty));
                    }
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

    fn place(&self, q: &Quote, qty: f64) -> OutcomeEvent {
        OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: q.exchange,
                symbol: self.symbol().clone(),
                side: q.side,
                order_type: OrderType::Limit { price: q.price, tif: TimeInForce::PostOnly },
                quantity: qty,
                reduce_only: q.reduce_only,
                client_order_id: String::new(),
            }],
            comment: format!(
                "funding_arb maker | {} {:?} qty={:.6} px={} reduce_only={}",
                q.exchange, q.side, qty, q.price, q.reduce_only
            ),
        }
    }

    /// 撤一张开着的单。下单请求还没结论的，框架丢弃这次撤单、受理的结论到了本策略重新判断时再撤；
    /// 撤单在途时不重发，撤单失败（多半是单刚成交、终态马上就到）后隔 `hedge_retry_ms` 再撤。
    fn cancel(&self, p: &PendingOrder, now: Timestamp) -> Option<OutcomeEvent> {
        if !p.is_open() {
            return None;
        }
        match &p.cancel {
            CancelState::None => {}
            CancelState::Sent => return None,
            CancelState::Failed { at, reason } => {
                if now.saturating_sub(*at) < self.p.hedge_retry_ms {
                    return None;
                }
                tracing::warn!(symbol = %self.symbol(), exchange = %p.order.exchange, client_order_id = %p.order.client_order_id, %reason, "撤单失败且单还开着，重撤");
            }
        }
        Some(OutcomeEvent::CancelOrder {
            exchange: p.order.exchange,
            symbol: self.symbol().clone(),
            client_order_id: p.order.client_order_id.clone(),
        })
    }

    /// 撤掉全部开着的 maker
    fn cancel_makers(&self, sv: SymbolView<'_>, now: Timestamp) -> Vec<OutcomeEvent> {
        sv.pending_orders()
            .filter(|p| matches!(p.order.order_type, OrderType::Limit { .. }))
            .filter_map(|p| self.cancel(p, now))
            .collect()
    }

    /// 持仓之和不为 0：撤所有 maker，本币所有单都结算完、且过了重试间隔时市价补齐。
    fn hedge(&mut self, sv: SymbolView<'_>, pair: LegPair, net: f64, positions: &[(Exchange, f64)], now: Timestamp) -> Vec<OutcomeEvent> {
        let out = self.cancel_makers(sv, now);
        // 等全部单结算完：开着的 maker 还可能成交、在途补腿单还没结果，此时补会补错量
        if sv.has_pending_orders() {
            return out;
        }
        let (target, side) = hedge_target(pair, net);
        if self.last_hedge_ts.is_some_and(|t| now.saturating_sub(t) < self.p.hedge_retry_ms) {
            return out;
        }
        let target_pos = positions.iter().find(|(e, _)| *e == target).map_or(0.0, |(_, p)| *p);
        // 补腿所持仓与净差同号、且够减 → 只减仓（平仓补过头时不会反手开仓）；
        // 不够减时不设，否则交易所会截短或整单拒绝，补不齐
        let reduce_only = target_pos.signum() == net.signum() && target_pos.abs() >= net.abs();
        if self.hedge_attempts > 0 {
            tracing::error!(
                symbol = %self.symbol(), %target, net, ?positions, attempts = self.hedge_attempts,
                "补腿后仍不平（补腿单失败或期间又有成交），重试"
            );
        }
        self.hedge_attempts += 1;
        self.last_hedge_ts = Some(now);
        let mut out = out;
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


    /// 看板与告警用：各所年化、资金费与盘口数据年龄、年化差、最近一次判断的信号（停止交易或补腿期间不再判断，
    /// 停在最后一次）、门槛、是否停止交易
    fn gauges(&self, now: Timestamp) -> Vec<Gauge> {
        let sym = self.symbol().as_str();
        let mut out = Vec::new();
        let mut known = Vec::new();
        for &ex in &self.exchanges {
            let ex_label = ex.to_string();
            let labels = [("symbol", sym), ("exchange", ex_label.as_str())];
            // 盘口年龄：公共流长时间断线或订阅静默无数据时，该币一直只撤不挂，日志里只有重连 warn（#767）。
            // 从没来过盘口的所从收到第一条事件算起（启动起就静默的订阅也看得见）
            if let Some(t) = self.bbo_local_ts.get(&ex).or(self.first_event_at.as_ref()) {
                out.push(Gauge::new("funding_arb_bbo_age_seconds", &labels, now.saturating_sub(*t) as f64 / 1000.0));
            }
            let Some(w) = self.funding.get(&ex) else { continue };
            out.push(Gauge::new("funding_arb_funding_age_seconds", &labels, now.saturating_sub(w.fetched_at) as f64 / 1000.0));
            if let Some(apr) = w.apr(now, self.p.max_funding_age_ms) {
                out.push(Gauge::new("funding_arb_apr", &labels, apr));
                known.push(apr);
            }
        }
        if known.len() == self.exchanges.len() {
            let max = known.iter().copied().fold(f64::MIN, f64::max);
            let min = known.iter().copied().fold(f64::MAX, f64::min);
            out.push(Gauge::new("funding_arb_apr_spread", &[("symbol", sym)], max - min));
        }
        if let Some(action) = self.last_action {
            let (name, pair) = match action {
                FundingAction::Open(p) => ("open", Some(p)),
                FundingAction::Close(p) => ("close", Some(p)),
                FundingAction::Hold => ("hold", None),
            };
            let sell = pair.map(|p| p.sell.to_string()).unwrap_or_default();
            let buy = pair.map(|p| p.buy.to_string()).unwrap_or_default();
            out.push(Gauge::new(
                "funding_arb_signal",
                &[("symbol", sym), ("action", name), ("sell", sell.as_str()), ("buy", buy.as_str())],
                1.0,
            ));
        }
        out.push(Gauge::new("funding_arb_open_threshold_apr", &[("symbol", sym)], self.p.open_threshold_apr));
        out.push(Gauge::new("funding_arb_close_threshold_apr", &[("symbol", sym)], self.p.close_threshold_apr));
        out.push(Gauge::new("funding_arb_halted", &[("symbol", sym)], if self.halted.is_some() { 1.0 } else { 0.0 }));
        out
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        let now = event.local_ts();
        self.first_event_at.get_or_insert(now);
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
                if let AccountData::Symbol(d) = &a.data {
                    self.note_order_outcome(d, now);
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

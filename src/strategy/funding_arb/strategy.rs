//! 多所资金费套利策略（单币一个实例，覆盖该币上市的全部所，至少两所）。
//!
//! 每个币各所持仓相加应为 0（对冲）。操作永远发生在**一对所**之间（见 [`LegPair`]）：
//! 开仓在年化差最大的两所，平仓在收益率跌破门槛的那对相反仓位，第三所不动。
//!
//! # 阶段（[`Phase`]）
//!
//! 每个事件先按各所持仓之和定阶段（转换只在 [`FundingArbStrategy::next_phase`] 一处），再按阶段做事：
//!
//! - **启动检查**：第一次有盘口时看各所持仓之和。为 0 进入挂单，不为 0 进入砍长腿。
//! - **砍长腿**（[`Phase::Trimming`]，#1038 用户定）：还没挂过单时各所持仓之和不为 0 —— 启动时就不平（常见于
//!   上一个进程退出时留下的挂单在重启前成交），或启动时只差零头、之后价格变动让它超过一笔最小单（#1270 用户定，
//!   取代「还没挂过单就不平则停止交易」：这时和刚启动一样不知道差额从哪来）。和多出来的方向同号的那些所叫长腿；
//!   在长腿上用只减仓市价单把多出来的量砍掉，不加短腿。
//!   长腿只有一所时直接砍；有几所时先砍资费最差的（多头那边年化最高、空头那边年化最低，口径同开平仓），
//!   不够再砍下一所，一次发齐；这几所的资金费有一所还未知就先等，期间不开仓。砍单失败隔 `hedge_retry_ms` 重试并记
//!   error。持仓之和回到零头以内即进入挂单（还没有所对）。
//! - **挂单**（[`Phase::Quoting`]）：已对冲。按各所过去 24h 年化决定开 / 平 / 持有（见 [`super::signal::decide`]），
//!   在选中的两所各维护一张只做 maker 的挂单（卖出所卖、买入所买；平仓只减仓），挂单的对齐交给 [`Makers`]。
//!   开仓前查两所各自的单币 / 账户杠杆，加仓超限不挂。记住最近一次挂单的所对，不平衡时在那里补。
//! - **补腿**（[`Phase::Hedging`]）：各所持仓之和不为 0（价值 ≥ 最小单额，且补腿所下得出这个量）。先撤掉全部挂单，
//!   等本币所有单都结束（撤单确认）后，在最近一次挂单的所对上市价补齐落后腿：多了就在该对的卖出所卖，少了就在
//!   买入所买。这正是用户规格「开仓加短腿、平仓砍长腿」：没成交的那条腿就是落后腿，补它即补齐这次操作；
//!   补过头（撤单期间另一边也成交）时反向补回的也是落后腿。补腿下单失败（被拒、或结果说不清超时）时，隔
//!   `hedge_retry_ms` 重试并记 error。持仓之和回到零头以内即回到挂单。
//! - **停止交易**（[`Phase::Halted`]）：只撤自己的挂单、不开不补，报 error 等人处理后重启。
//!
//! 一边成交即进入补腿，另一边挂单被撤、市价补齐，回到挂单 —— 这就是"一边成交撤另一腿、市价补腿"。
//!
//! # 停止交易
//!
//! 账户由本策略全权托管（用户定），不检测外来交易（#760 用户定）：账户里该币的全部持仓都当成
//! 自己的（框架持仓 = REST 基线 + 全部成交），别的单的成交按普通不平衡补；品种目录外的成交
//! 适配层直接丢弃（不进任何 symbol 的持仓）。
//!
//! 只在不知道该怎么补时停止交易：还没挂过单时不平，长腿各所的仓位却都小于一笔最小单、砍不出去（零头之外的
//! 罕见组合，如三所里两所各留一点同向零头）。
//!
//! # 成交与订单回报的先后
//!
//! 框架的订单列表只看订单回报，持仓只看成交（#899 用户定）。HL 的两路推送先后没保证：终态先到时持仓还没反映
//! 这笔成交，本策略会按旧持仓照常决策 —— 可能在同所再挂一张，或补一笔、成交到了再反向补回。结果仍对冲，
//! 只是多一笔挂单量（用户已接受），见测试 `hl_terminal_before_fill_hedges_twice_but_stays_hedged`。
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
use super::makers::{MakerRules, Makers, Quote};
use super::signal::{decide, FundingAction, LegPair, SettledFundingWindow};
use crate::domain::{
    from_f64, to_f64, Decimal, Exchange, OrderIntent, OrderType, Price, Quantity, Side, Symbol,
    SymbolMeta, Timestamp, BBO,
};
use crate::exchange::SubscriptionKind;
use crate::messaging::{IncomeEvent, MarketData, MarketEvent};
use crate::observability::Gauge;
use crate::strategy::{Signal, Strategy, StrategyView, SymbolView};
use std::collections::{HashMap, HashSet};

/// 本币此刻在做什么。只在 [`FundingArbStrategy::next_phase`] 里转换；每个阶段只带自己用得到的数据，
/// 「停止交易同时在补腿」「在补腿却没有所对」这类组合写不出来
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// 还没做过启动检查（第一次有盘口时做）
    Starting,
    /// 已对冲，按资金费挂单。`pair` = 最近一次挂单的所对（不平衡时在这对所补腿），还没挂过为 `None`
    Quoting { pair: Option<LegPair> },
    /// 各所持仓之和不为 0，在 `pair` 上补腿。`attempts` = 本轮已发出的补腿单数，>0 时再发即是重试
    Hedging { pair: LegPair, attempts: u32 },
    /// 启动时各所持仓之和不为 0，在长腿上砍掉多出来的量（#1038）。`attempts` = 已发出的砍单批数，>0 时再发即是重试
    Trimming { attempts: u32 },
    /// 停止交易（原因），只撤挂单、不开不补
    Halted { reason: String },
}

impl Phase {
    /// 不平衡时在哪对所补
    fn pair(&self) -> Option<LegPair> {
        match self {
            Phase::Quoting { pair } => *pair,
            Phase::Hedging { pair, .. } => Some(*pair),
            Phase::Starting | Phase::Trimming { .. } | Phase::Halted { .. } => None,
        }
    }
}

/// 本币在各所的持仓（带符号，正多负空），按所的固定顺序
#[derive(Debug)]
struct Legs(Vec<(Exchange, Quantity)>);

impl Legs {
    fn read(sv: SymbolView<'_>, exchanges: &[Exchange]) -> Self {
        Self(
            exchanges
                .iter()
                .map(|&ex| (ex, sv.position_size(ex)))
                .collect(),
        )
    }

    fn get(&self, ex: Exchange) -> Quantity {
        self.0
            .iter()
            .find(|(e, _)| *e == ex)
            .map_or(Quantity::ZERO, |(_, p)| *p)
    }

    /// 各所持仓之和（对冲时为 0）
    fn net(&self) -> Quantity {
        self.0.iter().map(|(_, p)| p).sum()
    }

    fn iter(&self) -> impl Iterator<Item = (Exchange, Quantity)> + '_ {
        self.0.iter().copied()
    }

    /// 平仓所对的可平量：卖出所持多、买入所持空（decide 保证），取两者绝对值较小的一个
    fn close_held(&self, pair: LegPair) -> Quantity {
        self.get(pair.sell)
            .max(Quantity::ZERO)
            .min((-self.get(pair.buy)).max(Quantity::ZERO))
    }
}

/// 只给日志与看板用的最近一次判断（不参与决策）
#[derive(Debug, Default)]
struct Readings {
    /// 收到第一条事件的时刻：某所从没来过盘口时，盘口年龄指标从这里算起
    first_event_at: Option<Timestamp>,
    /// 最近一次资金费信号（变化时打 info，进指标 `funding_arb_signal`）
    action: Option<FundingAction>,
    /// 该平仓、盘口也新鲜，可平量却凑不出两所都下得出的一手（进指标 `funding_arb_close_stuck`，要人看；
    /// 补腿或停止交易期间不判断，停在最后一次）
    close_stuck: bool,
    /// 有所的资金费未知（只在变成未知时打一次 warn）
    funding_unknown: bool,
}

pub struct FundingArbStrategy {
    p: PairParams,
    /// `p.order_distance` 的十进制形式：挂单价 = 盘口价 × (1 ± 它)
    distance: Decimal,
    /// 该币上市的所（固定顺序，决定同分时的取舍），至少两所
    exchanges: Vec<Exchange>,
    /// 各所合约规格：判断一个数量在该所下不下得出（精度与最小下单量的唯一出处）
    metas: HashMap<Exchange, SymbolMeta>,
    funding: HashMap<Exchange, SettledFundingWindow>,
    phase: Phase,
    /// 最近一次发补腿单（含启动砍长腿）的时刻：两次补腿至少隔 `hedge_retry_ms`（跨补腿轮次）
    last_hedge_at: Option<Timestamp>,
    makers: Makers,
    readings: Readings,
}

impl FundingArbStrategy {
    /// `metas`：该币在各所的合约规格，一所一份、至少两所，顺序即所的顺序（装配处按各所品种目录给出）
    pub fn new(p: PairParams, metas: Vec<SymbolMeta>) -> Self {
        let exchanges: Vec<Exchange> = metas.iter().map(|m| m.exchange).collect();
        let distinct: HashSet<_> = exchanges.iter().collect();
        assert!(
            exchanges.len() >= 2
                && distinct.len() == exchanges.len()
                && metas.iter().all(|m| m.symbol == p.symbol),
            "{}: 资金费套利至少要两个不同的所、且规格属于本币，实际 {:?}",
            p.symbol,
            metas
                .iter()
                .map(|m| (m.exchange, &m.symbol))
                .collect::<Vec<_>>()
        );
        let distance = from_f64(p.order_distance).unwrap_or_else(|| {
            panic!(
                "{}: order_distance 不是有限值：{}（配置校验应已拦下）",
                p.symbol, p.order_distance
            )
        });
        let makers = Makers::new(
            p.symbol.clone(),
            MakerRules {
                requote_tolerance: p.order_distance * p.requote_ratio,
                min_requote_ms: p.min_requote_ms,
                cancel_retry_ms: p.cancel_retry_ms,
                reject_backoff_ms: p.reject_backoff_ms,
            },
        );
        Self {
            p,
            distance,
            exchanges,
            metas: metas.into_iter().map(|m| (m.exchange, m)).collect(),
            funding: HashMap::new(),
            phase: Phase::Starting,
            last_hedge_at: None,
            makers,
            readings: Readings::default(),
        }
    }

    fn symbol(&self) -> &Symbol {
        &self.p.symbol
    }

    fn bbo_fresh(&self, sv: SymbolView<'_>, ex: Exchange, now: Timestamp) -> bool {
        sv.bbo_received_at(ex)
            .is_some_and(|t| now.saturating_sub(t) <= self.p.max_bbo_age_ms)
    }

    fn pair_fresh(&self, sv: SymbolView<'_>, pair: LegPair, now: Timestamp) -> bool {
        self.bbo_fresh(sv, pair.sell, now) && self.bbo_fresh(sv, pair.buy, now)
    }

    fn step(&mut self, sv: SymbolView<'_>, view: StrategyView<'_>, now: Timestamp) -> Vec<Signal> {
        if let Phase::Halted { .. } = self.phase {
            return self.makers.align(sv, &self.exchanges, &[], now);
        }

        // 不要求每所都有盘口：某所品种暂停、迟迟没盘口时，其余所的补腿和停机检查照常；
        // 挂单只在用到的两所盘口新鲜时才挂（见 quote 里的 pair_fresh）
        let bbos: HashMap<Exchange, &BBO> = self
            .exchanges
            .iter()
            .filter_map(|&ex| sv.bbo(ex).map(|b| (ex, b)))
            .collect();
        if bbos.is_empty() {
            return Vec::new();
        }
        // 参考价（币↔金额换算：挂单量、零头判定；杠杆闸门用各所自己的价）只用新鲜盘口；都不新鲜时
        // 退回全部盘口 —— 此时只用于零头判定和补腿，挂单另要求所对两所盘口新鲜，不会用到它
        let fresh_mids: Vec<Price> = bbos
            .iter()
            .filter(|(ex, _)| self.bbo_fresh(sv, **ex, now))
            .map(|(_, b)| b.mid_price())
            .collect();
        let mids: Vec<Price> = if fresh_mids.is_empty() {
            bbos.values().map(|b| b.mid_price()).collect()
        } else {
            fresh_mids
        };
        let px = mids.iter().sum::<Price>() / Decimal::from(mids.len());
        let legs = Legs::read(sv, &self.exchanges);

        self.phase = self.next_phase(&legs, px);
        match self.phase.clone() {
            Phase::Hedging { pair, attempts } => self.hedge(sv, pair, attempts, &legs, now),
            Phase::Trimming { attempts } => self.trim(sv, attempts, &legs, px, now),
            Phase::Quoting { pair } => self.quote(sv, view, pair, &bbos, &legs, px, now),
            Phase::Halted { .. } => self.makers.align(sv, &self.exchanges, &[], now),
            Phase::Starting => unreachable!("next_phase 不会停在启动检查"),
        }
    }

    /// 阶段转换（只此一处）：按各所持仓之和是不是零头，决定接下来在哪个阶段
    fn next_phase(&self, legs: &Legs, px: Price) -> Phase {
        let balanced = self.is_dust(legs.net(), px);
        let halt = |reason: String| {
            tracing::error!(symbol = %self.symbol(), %reason, "停止交易：撤掉本策略挂单，不再开仓或补腿，需人工处理后重启");
            Phase::Halted { reason }
        };
        match (&self.phase, balanced) {
            (Phase::Starting, true) => Phase::Quoting { pair: None },
            // 还没挂过单（启动时，或启动后一直没挂过）就不平：不知道差额从哪来，一律砍长腿（#1038、#1270）
            (Phase::Starting | Phase::Trimming { .. } | Phase::Quoting { pair: None }, false)
                if self.trim_candidates(legs, px).is_empty() =>
            {
                halt(format!(
                    "还没挂过单时各所持仓之和不为 0，长腿各所的仓位都小于一笔最小单、砍不出去：{legs:?}"
                ))
            }
            (Phase::Starting | Phase::Quoting { pair: None }, false) => {
                tracing::warn!(symbol = %self.symbol(), ?legs, "还没挂过单时各所持仓之和不为 0，砍长腿");
                Phase::Trimming { attempts: 0 }
            }
            (Phase::Trimming { attempts }, true) => {
                tracing::info!(symbol = %self.symbol(), attempts, ?legs, "不平已砍平，开始挂单");
                Phase::Quoting { pair: None }
            }
            (Phase::Quoting { pair: Some(pair) }, false) => Phase::Hedging {
                pair: *pair,
                attempts: 0,
            },
            (Phase::Hedging { pair, attempts }, true) => {
                if *attempts > 0 {
                    let net = legs.net();
                    if notional(net.abs(), px) >= self.min_notional(hedge_target(*pair, net).0) {
                        tracing::warn!(symbol = %self.symbol(), %net, ?legs, "补腿后剩下补腿所下不出的零头，不再补");
                    } else {
                        tracing::info!(symbol = %self.symbol(), attempts, "各所持仓已对冲");
                    }
                }
                Phase::Quoting { pair: Some(*pair) }
            }
            (
                phase @ (Phase::Quoting { .. }
                | Phase::Hedging { .. }
                | Phase::Trimming { .. }
                | Phase::Halted { .. }),
                _,
            ) => phase.clone(),
        }
    }

    /// 已对冲：按资金费定这一刻想挂的单，交给挂单对齐
    #[allow(clippy::too_many_arguments)]
    fn quote(
        &mut self,
        sv: SymbolView<'_>,
        view: StrategyView<'_>,
        pair: Option<LegPair>,
        bbos: &HashMap<Exchange, &BBO>,
        legs: &Legs,
        px: Price,
        now: Timestamp,
    ) -> Vec<Signal> {
        // 在途的补腿单（市价）成交前不挂 maker：它的成交会改变持仓
        if sv
            .pending_orders()
            .any(|p| matches!(p.order.order_type, OrderType::Market))
        {
            return Vec::new();
        }

        let action = self.funding_action(legs, px, now);
        let quotes = match action {
            FundingAction::Open(pair) if self.pair_fresh(sv, pair, now) => {
                self.open_quotes(pair, bbos, legs, px, view)
            }
            FundingAction::Close(pair) if self.pair_fresh(sv, pair, now) => {
                self.close_quotes(pair, bbos, legs, px)
            }
            _ => Vec::new(),
        };
        let sized = quotes.iter().any(|q| q.qty.is_some());
        let pair = match (action_pair(action), sized) {
            (Some(quoted), true) => Some(quoted),
            _ => pair,
        };
        self.phase = Phase::Quoting { pair };
        // 平仓信号在、盘口也新鲜，可平量本身却凑不出两所都下得出的一手（不看盘口厚度：第一档薄只是
        // 这一刻挂不出，下一跳就可能挂出，#799）：仓位会一直拿着，要人看
        let stuck = match action {
            FundingAction::Close(pair) => {
                self.pair_fresh(sv, pair, now)
                    && self.pair_qty(pair, legs.close_held(pair), px).is_none()
            }
            FundingAction::Open(_) | FundingAction::Hold => false,
        };
        if stuck && !self.readings.close_stuck {
            tracing::warn!(symbol = %self.symbol(), ?action, ?legs, "该平仓但可平量凑不出两所都下得出的一手（零头或下单粒度凑不齐），需人工处理");
        }
        self.readings.close_stuck = stuck;
        self.makers.align(sv, &self.exchanges, &quotes, now)
    }

    /// 各所年化 → 动作
    fn funding_action(&mut self, legs: &Legs, px: Price, now: Timestamp) -> FundingAction {
        let aprs: Vec<(Exchange, Option<f64>)> = self
            .exchanges
            .iter()
            .map(|&ex| (ex, self.apr(ex, now)))
            .collect();
        let held: Vec<(Exchange, Quantity)> = legs
            .iter()
            .filter(|(ex, p)| notional(p.abs(), px) >= self.min_notional(*ex))
            .collect();

        let unknown: Vec<Exchange> = aprs
            .iter()
            .filter(|(_, a)| a.is_none())
            .map(|(e, _)| *e)
            .collect();
        if unknown.is_empty() {
            self.readings.funding_unknown = false;
        } else if !self.readings.funding_unknown {
            self.readings.funding_unknown = true;
            tracing::warn!(symbol = %self.symbol(), ?unknown, "资金费数据未知或过期：不开仓，只平两所数据都在的仓位对");
        }

        let action = decide(
            &aprs,
            &held,
            self.p.open_threshold_apr,
            self.p.close_threshold_apr,
        );
        if self.readings.action != Some(action) {
            tracing::info!(symbol = %self.symbol(), ?aprs, ?held, ?action, "资金费信号变化");
        }
        self.readings.action = Some(action);
        action
    }

    /// 该所过去 24h 年化；没有数据或过期为 `None`
    fn apr(&self, ex: Exchange, now: Timestamp) -> Option<f64> {
        self.funding
            .get(&ex)
            .and_then(|w| w.apr(now, self.p.max_funding_age_ms))
    }

    /// 还没挂过单时不平，长腿上砍得出的所：与持仓之和同号的各所，可砍量 = min(该所持仓, 多出来的量)，
    /// 只留在该所下得出的（按所的固定顺序）
    fn trim_candidates(&self, legs: &Legs, px: Price) -> Vec<(Exchange, Quantity)> {
        let net = legs.net();
        legs.iter()
            .filter(|(_, p)| !p.is_zero() && (*p > Quantity::ZERO) == (net > Quantity::ZERO))
            .map(|(ex, p)| (ex, p.abs().min(net.abs())))
            .filter(|&(ex, q)| self.tradable(ex, q, px))
            .collect()
    }

    /// 本次挂单金额上限：min(配置, 两所吃单侧第一档金额)。吃单侧 = 一边成交后另一所补腿
    /// 要吃的那一侧：卖出所的单若没成交、要在它那里市价卖，看它的买一量；买入所同理看卖一量。
    fn cap_notional(&self, pair: LegPair, bbos: &HashMap<Exchange, &BBO>) -> f64 {
        let sell = bbos[&pair.sell];
        let buy = bbos[&pair.buy];
        self.p
            .order_notional
            .min(notional(sell.bid_qty, sell.bid_price))
            .min(notional(buy.ask_qty, buy.ask_price))
    }

    fn maker_price(&self, bbo: &BBO, side: Side) -> Price {
        match side {
            Side::Short => bbo.ask_price * (Decimal::ONE + self.distance),
            Side::Long => bbo.bid_price * (Decimal::ONE - self.distance),
        }
    }

    /// 持仓差是不是零头（不补、不停机），全策略只此一处判据：补不出去就是零头（见 [`Self::tradable`]）。
    /// 有所对时（见 [`Phase::pair`]）看补腿所下不下得出；还没有所对（启动时）时，只要有任一所下不出就算零头
    /// —— 本策略留下的零头不超过当时补腿所的一笔最小单，而那一所事后未知，按最粗的一所放宽，
    /// 免得每次重启都因自己的零头停机。代价：启动时不足最粗一所最小单的不平会被放过。
    fn is_dust(&self, net: Quantity, px: Price) -> bool {
        match self.phase.pair() {
            Some(pair) => !self.tradable(hedge_target(pair, net).0, net.abs(), px),
            None => !self
                .exchanges
                .iter()
                .all(|&ex| self.tradable(ex, net.abs(), px)),
        }
    }

    /// 该所实际用的最小下单金额：配置下限与交易所下限取大（用户 11-12 定）
    fn min_notional(&self, ex: Exchange) -> f64 {
        self.p.min_order_notional.max(self.metas[&ex].min_notional)
    }

    /// 一对所都下得出的最小金额
    fn pair_min_notional(&self, pair: LegPair) -> f64 {
        self.min_notional(pair.sell)
            .max(self.min_notional(pair.buy))
    }

    /// 这个币数量按参考价 `px` 在该所能不能下出去：取整后非 0、不低于最小下单量，
    /// 且取整后的金额不低于该所最小下单金额
    fn tradable(&self, ex: Exchange, qty: Quantity, px: Price) -> bool {
        let meta = &self.metas[&ex];
        meta.checked_exchange_qty(qty).is_ok()
            && notional(meta.round_coin_size_down(qty), px) >= self.min_notional(ex)
    }

    /// 两所都下得出的量：按两所精度依次向下取整（各所步长都是 10 的幂次，取整后同为两所步长的整数倍）；
    /// 任一所下不出则为 `None`
    fn pair_qty(&self, pair: LegPair, qty: Quantity, px: Price) -> Option<Quantity> {
        let qty = self.metas[&pair.buy]
            .round_coin_size_down(self.metas[&pair.sell].round_coin_size_down(qty));
        (self.tradable(pair.sell, qty, px) && self.tradable(pair.buy, qty, px)).then_some(qty)
    }

    /// 两所挂同样的量（取整见 [`Self::pair_qty`]）；任一所下不出则量为 `None`（不新挂，见 [`Quote::qty`]）。
    fn pair_quotes(
        &self,
        pair: LegPair,
        bbos: &HashMap<Exchange, &BBO>,
        qty: Quantity,
        px: Price,
        reduce_only: bool,
    ) -> Vec<Quote> {
        let qty = self.pair_qty(pair, qty, px);
        [(pair.sell, Side::Short), (pair.buy, Side::Long)]
            .into_iter()
            .map(|(ex, side)| Quote {
                exchange: ex,
                side,
                price: self.maker_price(bbos[&ex], side),
                qty,
                reduce_only,
            })
            .collect()
    }

    fn open_quotes(
        &self,
        pair: LegPair,
        bbos: &HashMap<Exchange, &BBO>,
        legs: &Legs,
        px: Price,
        view: StrategyView<'_>,
    ) -> Vec<Quote> {
        let cap = self.cap_notional(pair, bbos);
        if cap < self.pair_min_notional(pair) {
            return self.pair_quotes(pair, bbos, Quantity::ZERO, px, false);
        }
        let qty = qty_of(cap, px);
        for (ex, delta) in [(pair.sell, -qty), (pair.buy, qty)] {
            // 超限和「定不出量」同一处理：不新挂，已挂的按价格容差留着。持仓卡在上限边上时闸门随盘口
            // 小跳来回翻，超限就撤会挂了即撤、白耗下单撤单次数（#769）。已挂的单是在放行时按成交后不超限
            // 定的量，留着成交只会因之后的价格、净值波动略超一点
            if !self.leverage_allows(view, ex, legs.get(ex), delta, bbos[&ex].mid_price()) {
                return self.pair_quotes(pair, bbos, Quantity::ZERO, px, false);
            }
        }
        // 不设只减仓：开仓可能顺带削减该所已有的腿（用户允许），并可能越过 0 变成反向仓
        self.pair_quotes(pair, bbos, qty, px, false)
    }

    fn close_quotes(
        &self,
        pair: LegPair,
        bbos: &HashMap<Exchange, &BBO>,
        legs: &Legs,
        px: Price,
    ) -> Vec<Quote> {
        let qty = qty_of(self.cap_notional(pair, bbos), px).min(legs.close_held(pair));
        self.pair_quotes(pair, bbos, qty, px, true)
    }

    /// 在该所把持仓从 `pos` 改到 `pos + delta` 后，单币与账户杠杆是否仍在上限内。
    /// `px` 是该所自己的盘口中间价：杠杆每所各算，用该所的价才和交易所、账本里别的币的口径一致
    /// （几所均价会把价高的所低估，#769）。
    /// 持仓绝对值不增（削减已有腿）时总是允许 —— 超限只停止开仓，不拦减仓。
    /// 净值或持仓总价值未知时不许加仓。
    ///
    /// 分母见 [`Self::leverage_base`]；同所别的币与本币同一计价币（每所只接一种计价币）。
    ///
    /// 账户持仓总价值 = 本地持仓账本里该所**别的币**的持仓价值之和 + 本币开仓后的价值（#759，用户定：
    /// 本地所有持仓价值相加）。本币用自己这一刻的持仓，不用账本那份（可能还没含刚到的成交）；
    /// 别的币的成交由账本在成交后立刻发出，不等交易所轮询。只算配置里的币，账户里别的仓位不计。
    /// 杠杆的分母：该所净值，只在它与本币合约计价币同一币种、且为正时可用。币种对不上（如交易所
    /// 给的是折成美元的数）当作未知、不按 1:1 硬除。开仓闸门和看板读数都从这里取，看到同一个数。
    fn leverage_base(&self, view: StrategyView<'_>, ex: Exchange) -> Option<f64> {
        view.equity(ex)
            .filter(|e| e.asset == self.metas[&ex].quote && e.amount > 0.0)
            .map(|e| e.amount)
    }

    fn leverage_allows(
        &self,
        view: StrategyView<'_>,
        ex: Exchange,
        pos: Quantity,
        delta: Quantity,
        px: Price,
    ) -> bool {
        let before = notional(pos.abs(), px);
        let after = notional((pos + delta).abs(), px);
        if after <= before {
            return true;
        }
        let (Some(equity), Some(values)) = (self.leverage_base(view, ex), view.position_values(ex))
        else {
            return false;
        };
        let others: f64 = values
            .iter()
            .filter(|(s, _)| *s != self.symbol())
            .map(|(_, leg)| leg.value())
            .sum();
        let account_after = others + after;
        let ok = after / equity <= self.p.max_symbol_leverage
            && account_after / equity <= self.p.max_account_leverage;
        if !ok {
            tracing::debug!(symbol = %self.symbol(), %ex, equity, others, before, after, "杠杆到上限，不再开仓");
        }
        ok
    }

    /// 补腿：撤所有 maker，本币所有单都结束、且过了重试间隔时市价补齐。
    fn hedge(
        &mut self,
        sv: SymbolView<'_>,
        pair: LegPair,
        attempts: u32,
        legs: &Legs,
        now: Timestamp,
    ) -> Vec<Signal> {
        let mut out = self.makers.cancel_all(sv, now);
        // 等全部单结束：开着的 maker 还可能成交、在途补腿单还没结果，此时补会补错量
        if sv.has_pending_orders() {
            return out;
        }
        let net = legs.net();
        let (target, side) = hedge_target(pair, net);
        if self
            .last_hedge_at
            .is_some_and(|t| now.saturating_sub(t) < self.p.hedge_retry_ms)
        {
            return out;
        }
        let target_pos = legs.get(target);
        // 补腿所持仓与净差同号、且够减 → 只减仓（平仓补过头时不会反手开仓）；
        // 不够减时不设，否则交易所会截短或整单拒绝，补不齐
        let reduce_only = (target_pos > Quantity::ZERO) == (net > Quantity::ZERO)
            && target_pos.abs() >= net.abs();
        if attempts > 0 {
            tracing::error!(
                symbol = %self.symbol(), %target, %net, ?legs, attempts,
                "补腿后仍不平（补腿单失败或期间又有成交），重试"
            );
        }
        self.phase = Phase::Hedging {
            pair,
            attempts: attempts + 1,
        };
        self.last_hedge_at = Some(now);
        out.push(Signal::PlaceOrders {
            orders: vec![OrderIntent {
                exchange: target,
                symbol: self.symbol().clone(),
                side,
                order_type: OrderType::Market,
                quantity: net.abs(),
                reduce_only,
            }],
            comment: format!(
                "funding_arb hedge | {target} {side:?} qty={} positions={legs:?} pair={pair:?}",
                net.abs()
            ),
        });
        out
    }

    /// 砍长腿：撤所有 maker，本币所有单都结束、且过了重试间隔时，在长腿上用只减仓市价单砍掉多出来的量。
    /// 长腿有几所时按资费从差到好排（多头所年化高的先砍、空头所年化低的先砍，同分按所的固定顺序），
    /// 依次分配，一次发齐；分到的量下不出的那所跳过，最后剩下下不出的就是零头。
    fn trim(
        &mut self,
        sv: SymbolView<'_>,
        attempts: u32,
        legs: &Legs,
        px: Price,
        now: Timestamp,
    ) -> Vec<Signal> {
        let mut out = self.makers.cancel_all(sv, now);
        if sv.has_pending_orders()
            || self
                .last_hedge_at
                .is_some_and(|t| now.saturating_sub(t) < self.p.hedge_retry_ms)
        {
            return out;
        }
        let net = legs.net();
        let long = net > Quantity::ZERO;
        let mut candidates = self.trim_candidates(legs, px);
        if candidates.len() > 1 {
            let Some(aprs) = candidates
                .iter()
                .map(|&(ex, _)| self.apr(ex, now))
                .collect::<Option<Vec<f64>>>()
            else {
                if !self.readings.funding_unknown {
                    self.readings.funding_unknown = true;
                    tracing::warn!(symbol = %self.symbol(), ?legs, "还没挂过单时不平、长腿有几所，资金费还未知：等数据到了再砍");
                }
                return out;
            };
            // 资费最差 = 这条腿付得最多：多头看年化高，空头看年化低。稳定排序，同分保持所的顺序
            let mut ranked: Vec<_> = candidates.into_iter().zip(aprs).collect();
            ranked.sort_by(|(_, a), (_, b)| {
                let worst_first = b.total_cmp(a);
                if long {
                    worst_first
                } else {
                    worst_first.reverse()
                }
            });
            candidates = ranked.into_iter().map(|(c, _)| c).collect();
        }
        let side = if long { Side::Short } else { Side::Long };
        let mut remaining = net.abs();
        let mut orders = Vec::new();
        for (ex, held) in candidates {
            let qty = held.min(remaining);
            if self.tradable(ex, qty, px) {
                remaining -= qty;
                orders.push(OrderIntent {
                    exchange: ex,
                    symbol: self.symbol().clone(),
                    side,
                    order_type: OrderType::Market,
                    quantity: qty,
                    reduce_only: true,
                });
            }
        }
        if attempts > 0 {
            tracing::error!(
                symbol = %self.symbol(), %net, ?legs, attempts,
                "砍长腿后仍不平（砍单失败或期间又有成交），重试"
            );
        }
        self.phase = Phase::Trimming {
            attempts: attempts + 1,
        };
        self.last_hedge_at = Some(now);
        let summary: Vec<_> = orders.iter().map(|o| (o.exchange, o.quantity)).collect();
        out.push(Signal::PlaceOrders {
            orders,
            comment: format!(
                "funding_arb trim | {side:?} {summary:?} net={net} positions={legs:?}"
            ),
        });
        out
    }
}

/// 多了在卖出所卖、少了在买入所买：补的就是这次操作里没成交的落后腿
fn hedge_target(pair: LegPair, net: Quantity) -> (Exchange, Side) {
    if net > Quantity::ZERO {
        (pair.sell, Side::Short)
    } else {
        (pair.buy, Side::Long)
    }
}

/// 数量 × 价格 → 金额（计价币）
fn notional(qty: Quantity, px: Price) -> f64 {
    to_f64(qty * px)
}

/// 金额 ÷ 价格 → 意图数量（未按步长取整，下单前经 [`SymbolMeta`] 取整）。
/// 价格为 0（盘口异常）时定不出量，返回 0（不挂）
fn qty_of(notional: f64, px: Price) -> Quantity {
    let notional = from_f64(notional).unwrap_or_else(|| panic!("金额不是有限值：{notional}"));
    notional.checked_div(px).unwrap_or(Quantity::ZERO)
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
            .map(|&ex| {
                (
                    ex,
                    HashSet::from([SubscriptionKind::BBO {
                        symbol: self.symbol().clone(),
                    }]),
                )
            })
            .collect()
    }

    /// 看板与告警用：各所年化、资金费与盘口数据年龄、年化差、单币杠杆、跨所净敞口、是否单腿、
    /// 最近一次判断的信号（停止交易或补腿期间不再判断，
    /// 停在最后一次）、门槛、是否停止交易
    fn gauges(&self, view: StrategyView<'_>, now: Timestamp) -> Vec<Gauge> {
        let sym = self.symbol().as_str();
        let mut out = Vec::new();
        let mut known = Vec::new();
        // 各所带符号持仓价值（账本已发读数的所）、有仓的所数
        let mut legs: Vec<f64> = Vec::new();
        let mut held = 0usize;
        for &ex in &self.exchanges {
            let ex_label = ex.to_string();
            let labels = [("symbol", sym), ("exchange", ex_label.as_str())];
            // 盘口年龄：公共流长时间断线或订阅静默无数据时，该币一直只撤不挂，日志里只有重连 warn（#767）。
            // 从没来过盘口的所从收到第一条事件算起（启动起就静默的订阅也看得见）
            let received = view
                .symbol(self.symbol())
                .and_then(|sv| sv.bbo_received_at(ex));
            if let Some(t) = received.or(self.readings.first_event_at) {
                out.push(Gauge::new(
                    "funding_arb_bbo_age_seconds",
                    &labels,
                    now.saturating_sub(t) as f64 / 1000.0,
                ));
            }
            // 持仓：取账本读数（与杠杆闸门同一来源）
            if let Some(leg) = view.position_values(ex).and_then(|v| v.get(self.symbol())) {
                legs.push(leg.signed_value());
                if !leg.size().is_zero() {
                    held += 1;
                }
                // 单币杠杆 = 本币在该所的持仓价值 ÷ 该所净值，与开仓闸门同一口径（只除同一币种）。
                // 账户独占，该所全部持仓都是各币实例的：按所把各币相加就是闸门的账户杠杆
                if let Some(equity) = self.leverage_base(view, ex) {
                    out.push(Gauge::new(
                        "funding_arb_symbol_leverage",
                        &labels,
                        leg.value() / equity,
                    ));
                }
            }
            let Some(w) = self.funding.get(&ex) else {
                continue;
            };
            out.push(Gauge::new(
                "funding_arb_funding_age_seconds",
                &labels,
                now.saturating_sub(w.fetched_at) as f64 / 1000.0,
            ));
            if let Some(apr) = w.apr(now, self.p.max_funding_age_ms) {
                out.push(Gauge::new("funding_arb_apr", &labels, apr));
                known.push(apr);
            }
        }
        // 跨所净敞口：各所持仓价值直接相加，USDT 与 USDC 按 1:1 算（本策略的口径，#841），
        // 对冲到位时接近 0。有所还没有读数时不推（未知，不是 0）
        if legs.len() == self.exchanges.len() {
            out.push(Gauge::new(
                "funding_arb_net_notional",
                &[("symbol", sym)],
                legs.iter().sum(),
            ));
            out.push(Gauge::new(
                "funding_arb_single_leg",
                &[("symbol", sym)],
                if held == 1 { 1.0 } else { 0.0 },
            ));
        }
        if known.len() == self.exchanges.len() {
            let max = known.iter().copied().fold(f64::MIN, f64::max);
            let min = known.iter().copied().fold(f64::MAX, f64::min);
            out.push(Gauge::new(
                "funding_arb_apr_spread",
                &[("symbol", sym)],
                max - min,
            ));
        }
        if let Some(action) = self.readings.action {
            let (name, pair) = match action {
                FundingAction::Open(p) => ("open", Some(p)),
                FundingAction::Close(p) => ("close", Some(p)),
                FundingAction::Hold => ("hold", None),
            };
            let sell = pair.map(|p| p.sell.to_string()).unwrap_or_default();
            let buy = pair.map(|p| p.buy.to_string()).unwrap_or_default();
            out.push(Gauge::new(
                "funding_arb_signal",
                &[
                    ("symbol", sym),
                    ("action", name),
                    ("sell", sell.as_str()),
                    ("buy", buy.as_str()),
                ],
                1.0,
            ));
        }
        out.push(Gauge::new(
            "funding_arb_open_threshold_apr",
            &[("symbol", sym)],
            self.p.open_threshold_apr,
        ));
        out.push(Gauge::new(
            "funding_arb_close_threshold_apr",
            &[("symbol", sym)],
            self.p.close_threshold_apr,
        ));
        out.push(Gauge::new(
            "funding_arb_close_stuck",
            &[("symbol", sym)],
            if self.readings.close_stuck { 1.0 } else { 0.0 },
        ));
        out.push(Gauge::new(
            "funding_arb_halted",
            &[("symbol", sym)],
            if matches!(self.phase, Phase::Halted { .. }) {
                1.0
            } else {
                0.0
            },
        ));
        out
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<Signal> {
        let now = event.local_ts();
        self.readings.first_event_at.get_or_insert(now);
        // 分发层只投本币的事件，这里不再判 symbol（P4）
        if let IncomeEvent::Market(MarketEvent {
            data: MarketData::Custom(c),
            ..
        }) = event
        {
            if let Some(w) = c.get::<SettledFundingWindow>() {
                self.funding.insert(w.exchange, w.clone());
            }
        }
        // 视图只含本实例订阅范围（本币）的单
        self.makers.on_notices(view.order_notices(), now);
        let Some(sv) = view.symbol(self.symbol()) else {
            return Vec::new();
        };
        self.step(sv, view, now)
    }
}

/// 阶段转换表：从每个阶段出发，持仓平 / 不平各落到哪（场景测试见 tests/funding_arb_strategy.rs）
#[cfg(test)]
mod phase_tests {
    use super::*;
    use crate::domain::{PositiveDecimal, PriceStep};
    use rust_decimal_macros::dec;
    use std::sync::Arc;

    const BN: Exchange = Exchange::Binance;
    const HL: Exchange = Exchange::Hyperliquid;
    const PAIR: LegPair = LegPair { sell: BN, buy: HL };

    fn strategy(phase: Phase) -> FundingArbStrategy {
        let p = PairParams {
            symbol: "XMR".to_string(),
            open_threshold_apr: 0.5,
            close_threshold_apr: 0.1,
            order_notional: 200.0,
            min_order_notional: 11.0,
            max_account_leverage: 1.5,
            max_symbol_leverage: 0.2,
            order_distance: 0.001,
            requote_ratio: 1.0,
            hedge_retry_ms: 1000,
            cancel_retry_ms: 1000,
            reject_backoff_ms: 1000,
            min_requote_ms: 0,
            max_bbo_age_ms: 5000,
            max_funding_age_ms: 7_200_000,
        };
        let meta = |exchange| SymbolMeta {
            exchange,
            symbol: "XMR".to_string(),
            price_rule: Arc::new(PriceStep::new(PositiveDecimal::new(dec!(0.01)).unwrap())),
            size_step: PositiveDecimal::new(dec!(0.001)).unwrap(),
            min_order_size: PositiveDecimal::new(dec!(0.001)).unwrap(),
            min_notional: 0.0,
            contract_size: PositiveDecimal::new(dec!(1)).unwrap(),
            quote: "USDT".to_string(),
        };
        let mut s = FundingArbStrategy::new(p, vec![meta(BN), meta(HL)]);
        s.phase = phase;
        s
    }

    /// 价格 100：差 1 个币 = 100U 不平；差 0.05 个币 = 5U 低于最小单 11U，算零头
    fn legs(bn: Decimal, hl: Decimal) -> Legs {
        Legs(vec![(BN, bn), (HL, hl)])
    }

    fn next(from: Phase, bn: Decimal, hl: Decimal) -> Phase {
        strategy(from).next_phase(&legs(bn, hl), dec!(100))
    }

    #[test]
    fn every_phase_transition() {
        let halted = |p: &Phase| matches!(p, Phase::Halted { .. });
        let hedging = |attempts| Phase::Hedging {
            pair: PAIR,
            attempts,
        };
        let quoting = |pair| Phase::Quoting { pair };
        let (flat, dust, off) = (
            (dec!(-1), dec!(1)),
            (dec!(-1), dec!(1.05)),
            (dec!(-1), dec!(2)),
        );

        // 启动检查：平（含零头）→ 挂单、没有所对；不平 → 砍长腿；长腿砍不出（两所同向各多 6U，合计 12U 超过
        // 最小单 11U，单所却都不到）→ 停止交易
        let stuck = (dec!(0.06), dec!(0.06));
        for (bn, hl) in [flat, dust] {
            assert_eq!(next(Phase::Starting, bn, hl), quoting(None));
        }
        assert_eq!(
            next(Phase::Starting, off.0, off.1),
            Phase::Trimming { attempts: 0 }
        );
        assert!(halted(&next(Phase::Starting, stuck.0, stuck.1)));

        // 砍长腿：不平 → 继续（次数不变）；平 → 挂单、没有所对
        let trimming = Phase::Trimming { attempts: 1 };
        assert_eq!(next(trimming.clone(), off.0, off.1), trimming);
        for (bn, hl) in [flat, dust] {
            assert_eq!(next(trimming.clone(), bn, hl), quoting(None));
        }

        // 挂单：平 → 不变；不平 → 有所对就补腿（本轮从 0 次起），没挂过单就和启动时一样砍长腿（#1270），
        // 长腿砍不出才停止交易
        for pair in [None, Some(PAIR)] {
            assert_eq!(next(quoting(pair), flat.0, flat.1), quoting(pair));
        }
        assert_eq!(next(quoting(Some(PAIR)), off.0, off.1), hedging(0));
        assert_eq!(
            next(quoting(None), off.0, off.1),
            Phase::Trimming { attempts: 0 }
        );
        assert!(halted(&next(quoting(None), stuck.0, stuck.1)));

        // 补腿：不平 → 继续（次数不变，发单时才加）；平 → 回到挂单，记住所对
        assert_eq!(next(hedging(2), off.0, off.1), hedging(2));
        for (bn, hl) in [flat, dust] {
            assert_eq!(next(hedging(2), bn, hl), quoting(Some(PAIR)));
        }

        // 停止交易：不再离开
        for (bn, hl) in [flat, off] {
            let from = Phase::Halted { reason: "x".into() };
            assert_eq!(next(from.clone(), bn, hl), from);
        }
    }
}

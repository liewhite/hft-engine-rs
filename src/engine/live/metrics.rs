//! Metrics - 交易指标观测
//!
//! 订阅账户总线的**全量**事件流，聚合后定时输出结构化日志：
//! - **账户**：各交易所净值、本地账本的持仓价值（都带币种）
//! - **持仓**：各币在各所的仓位与持仓价值
//! - **订单**：当前挂单数、累计拒单数
//! - **历史**：成交笔数、成交额、手续费、本次会话盈亏、强平次数、过去 24h 资金费
//!
//! 设计要点：
//! - **只传事实，不做汇总判断**（#841）：每个金额都按「所 + 币种」给出，不同币种不相加、
//!   不同所不合并；杠杆、跨所净敞口、单腿这些是策略的口径，由策略自己推（[`crate::strategy::Strategy::gauges`]）
//! - **持仓与持仓价值只读持仓账本发的读数**（[`PositionValues`]，#812）：看板上的持仓与
//!   策略风控闸门看到的是同一个数，本模块不自己折叠成交、不自己估值。价值的币种是该币合约的
//!   计价币（投产注册时随合约规格给到）
//! - **不读取任何 executor 的内部状态**，与策略层零耦合
//! - 盈亏为**本次会话**口径，按 (币, 所) 一条腿算：现金流 + 相对会话起始仓位的存货估值变化，
//!   不含手续费（手续费币种不一定是计价币，按币种另推 `hft_fee_total{asset}`，#817），
//!   这样重启后不会把重启前就持有的存货算成假盈利
//! - 默认只输出日志；配置 `PUSHGATEWAY_URL` 后同一份数字推送 pushgateway
//!   （见 [`crate::observability`]，未配置时无外部依赖）

use super::{ExecutorId, OrderRequestCounts};
use crate::domain::{
    now_ms, to_f64, AccountId, Exchange, Fill, Money, Quantity, Symbol, SymbolMeta, Timestamp,
    TradingStats,
};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, LegValue, PositionBaseline,
    PositionValues, RequestOutcome, SymbolAccountData,
};
use crate::observability::{Gauge, MetricsPushConfig, PromText, Pusher};
use crate::tasks::{Addr, Gone, Group, Inbox, Scope, Task};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

/// 默认指标报告间隔
pub const DEFAULT_REPORT_INTERVAL_MS: u64 = 60_000;

/// 资金费收入的统计窗口：过去 24h（交易所的资费轮询器每次也是重发最近 24h，靠结算 ID 去重）
const FUNDING_FEE_WINDOW_MS: u64 = 24 * 3600 * 1000;

/// 日志里保留的最近成交条数（排查用，`RUST_LOG=metrics=debug` 才输出）
const RECENT_FILL_CAPACITY: usize = 50;

/// 一条腿：一个币在一个所
type Leg = (Symbol, Exchange);

/// Metrics 构造参数
pub struct MetricsArgs {
    /// 报告间隔 (毫秒)
    pub interval_ms: u64,
    /// 可选的 Prometheus pushgateway 推送（None = 只输出日志）
    pub push: Option<MetricsPushConfig>,
    /// 订阅：全量账户事件（实盘 + 全部模拟账户一次订阅，按事件自带的账户分账；
    /// 持仓读数也在这条总线上，由持仓账本发）
    pub account_bus: AccountBus,
    /// 实盘网关的下单 / 撤单请求计数（网关写、这里只读，见 [`OrderRequestCounts`]）
    pub order_requests: Arc<OrderRequestCounts>,
}

/// 从事件流重建的聚合视图（纯状态，不含收件与定时；单测直接构造）
struct MetricsView {
    /// 各所交易所报的净值（实盘，带币种）
    equities: HashMap<Exchange, Money>,
    /// 持仓账本发的最新持仓读数：所 -> 币 -> 读数（实盘 `PositionValues`，见模块文档）
    positions: HashMap<Exchange, HashMap<Symbol, LegValue>>,
    /// 交易所说还开着的本引擎挂单：symbol -> (所, client_order_id)。按订单回报增删（非终态加、终态删）
    open_orders: HashMap<Symbol, HashSet<(Exchange, String)>>,
    /// 正在跟踪的腿及其计价币（由 [`MetricsHandle::register_legs`] 注册）。不在这里的腿的事件
    /// 不计入，见 [`Self::untracked_events`]
    quotes: HashMap<Leg, String>,
    /// 会话起始仓位基线（取自投产握手，只写一次）。用于把重启前的存货从盈亏里剔除；缺失视为 0
    baseline: HashMap<Leg, Quantity>,
    /// 各腿本次会话的累计成交统计
    per_leg: HashMap<Leg, TradingStats>,
    /// 最近成交（排查用，只进 debug 日志）
    recent_fills: VecDeque<Fill>,
    /// 落在跟踪范围外、被跳过的事件数
    ///
    /// **分桶部署下这个数持续增长是正常的**：多实例共用同一账户时，私有流会把其他桶的
    /// symbol 事件也推给本实例。若为单实例全量部署且此数持续增长，才说明注册范围有问题。
    untracked_events: u64,
    /// 模拟账户累计成交统计（来自账户总线上带 Paper 标签的事件），与实盘视图分账。
    /// 模拟柜台的现金与手续费都以 [`crate::sim::SIM_CASH_ASSET`] 计
    paper_stats: HashMap<AccountId, TradingStats>,
    /// 各模拟账户柜台的最新净值：(账户, 所) -> 净值。
    /// 已知取舍：账户撤下后条目残留（= 最后一次快照，持续上报），生命周期内不清理
    paper_equities: HashMap<(AccountId, Exchange), Money>,
    /// 各策略实例最新一份自报读数（见 [`crate::strategy::Strategy::gauges`]），按实例归属；
    /// 实例撤下时交来空的一份，条目即删除
    strategy_gauges: HashMap<ExecutorId, (AccountId, Vec<Gauge>)>,
    /// 实盘账户收到 / 付出的资金费：(所, 结算 ID) -> (币, 金额, 结算时刻)。
    /// 交易所轮询器每次重发最近 24h，同一笔按结算 ID 只记一次；超出窗口的在报告时清掉
    funding_fees: HashMap<(Exchange, u64), (Symbol, Money, Timestamp)>,
    /// 本上报周期内各账户策略实例消费事件的最大滞后（毫秒），每次报告后清零
    event_lag_max_ms: HashMap<AccountId, u64>,
    /// 实盘网关请求计数（只读）
    order_requests: Arc<OrderRequestCounts>,
}

impl MetricsView {
    fn new(order_requests: Arc<OrderRequestCounts>) -> Self {
        Self {
            equities: HashMap::new(),
            positions: HashMap::new(),
            open_orders: HashMap::new(),
            quotes: HashMap::new(),
            baseline: HashMap::new(),
            per_leg: HashMap::new(),
            recent_fills: VecDeque::new(),
            untracked_events: 0,
            paper_stats: HashMap::new(),
            paper_equities: HashMap::new(),
            strategy_gauges: HashMap::new(),
            funding_fees: HashMap::new(),
            event_lag_max_ms: HashMap::new(),
            order_requests,
        }
    }

    /// 某实例报来的一拍内最大事件滞后，按账户取本周期最大值
    fn record_event_lag(&mut self, account: AccountId, max_lag_ms: u64) {
        let v = self.event_lag_max_ms.entry(account).or_default();
        *v = (*v).max(max_lag_ms);
    }

    /// 投产注册：跟踪的腿（带计价币）+ 会话起始仓位基线（见 [`MetricsHandle::register_legs`]）
    fn register(&mut self, metas: Vec<SymbolMeta>, baselines: Vec<PositionBaseline>) {
        tracing::info!(count = metas.len(), "Metrics tracking legs");
        for m in metas {
            self.quotes.insert((m.symbol, m.exchange), m.quote);
        }
        for b in baselines {
            self.baseline
                .insert((b.position.symbol, b.position.exchange), b.position.size);
        }
    }

    /// 账户事件入口：实盘进实盘视图，模拟账户按账户分账 —— 分道由事件自带的账户标签决定
    fn on_account(&mut self, msg: AccountEvent) {
        // 穷举而非 `!= Live`：新增账户类型时这里编译失败
        match msg.account {
            AccountId::Paper(_) => {
                self.apply_paper_event(&msg.account.clone(), &msg.data);
                return;
            }
            AccountId::Live => {}
        }
        let data = match msg.data {
            AccountData::Symbol(d) => d,
            AccountData::Exchange(ExchangeAccountData::Equity { exchange, equity }) => {
                self.equities.insert(exchange, equity);
                return;
            }
            AccountData::Exchange(ExchangeAccountData::PositionValues(PositionValues {
                exchange,
                legs,
            })) => {
                self.positions.insert(exchange, legs);
                return;
            }
            AccountData::Exchange(_) => return,
        };
        let leg = (data.symbol().clone(), data.exchange());
        if !self.quotes.contains_key(&leg) {
            self.untracked_events += 1;
            return;
        }
        match data {
            SymbolAccountData::Fill(fill) => {
                self.leg_stats_mut(leg).apply_fill(&fill);
                if self.recent_fills.len() == RECENT_FILL_CAPACITY {
                    self.recent_fills.pop_front();
                }
                self.recent_fills.push_back(fill);
            }
            SymbolAccountData::FundingFee(fee) => {
                self.funding_fees.insert(
                    (fee.exchange, fee.tran_id),
                    (fee.symbol, fee.amount, fee.timestamp),
                );
            }
            SymbolAccountData::OrderUpdate(update) => {
                self.leg_stats_mut(leg).apply_order_update(&update);
                self.track_open_order(&update);
            }
            SymbolAccountData::OrderRequest(r) => {
                if matches!(r.outcome, RequestOutcome::PlaceRejected { .. }) {
                    self.leg_stats_mut(leg).apply_place_rejected();
                }
            }
        }
    }

    fn leg_stats_mut(&mut self, leg: Leg) -> &mut TradingStats {
        self.per_leg.entry(leg).or_default()
    }

    /// 按交易所回报维护「还开着的本引擎挂单」
    fn track_open_order(&mut self, update: &crate::domain::OrderUpdate) {
        let Some(cid) = &update.client_order_id else {
            return;
        };
        let orders = self.open_orders.entry(update.symbol.clone()).or_default();
        let key = (update.exchange, cid.clone());
        if update.status.is_terminal() {
            orders.remove(&key);
        } else {
            orders.insert(key);
        }
    }

    /// 模拟账户事件的分账逻辑（纯状态转移，供 handler 委托与单测直调）。
    /// **绝不**触碰实盘视图（per_leg / positions / equities）。
    fn apply_paper_event(&mut self, account: &AccountId, data: &AccountData) {
        match data {
            AccountData::Symbol(SymbolAccountData::Fill(fill)) => {
                self.paper_stats_mut(account).apply_fill(fill);
            }
            AccountData::Symbol(SymbolAccountData::OrderUpdate(update)) => {
                self.paper_stats_mut(account).apply_order_update(update);
            }
            AccountData::Exchange(ExchangeAccountData::Equity { exchange, equity }) => {
                self.paper_equities
                    .insert((account.clone(), *exchange), equity.clone());
            }
            _ => {}
        }
    }

    fn paper_stats_mut(&mut self, account: &AccountId) -> &mut TradingStats {
        self.paper_stats.entry(account.clone()).or_default()
    }

    /// 清掉结算时刻已出 24h 窗口的资金费
    fn prune_funding_fees(&mut self, now: Timestamp) {
        self.funding_fees
            .retain(|_, (_, _, ts)| now.saturating_sub(*ts) <= FUNDING_FEE_WINDOW_MS);
    }

    /// 收一个策略实例最新的自报读数；空 = 实例已撤下，删掉它的条目
    fn set_strategy_gauges(
        &mut self,
        executor: ExecutorId,
        account: AccountId,
        gauges: Vec<Gauge>,
    ) {
        if gauges.is_empty() {
            self.strategy_gauges.remove(&executor);
        } else {
            self.strategy_gauges.insert(executor, (account, gauges));
        }
    }

    /// 输出一次完整报告（结构化日志），返回同一份数字的推送文本
    fn report(&mut self) -> PromText {
        self.prune_funding_fees(now_ms());
        let prom = self.render();
        // 滞后按上报周期取最大：这一份已报，下一周期重新累计
        self.event_lag_max_ms.clear();
        let forced: u64 = self.per_leg.values().map(|s| s.forced_fills).sum();
        if forced > 0 {
            tracing::warn!(
                target: "metrics",
                forced_fills = forced,
                "metrics.forced_fills：存在强平/ADL 成交"
            );
        }

        // 最近成交明细放 debug：默认不刷屏，排查时开 RUST_LOG=metrics=debug 即可看到
        for fill in &self.recent_fills {
            tracing::debug!(
                target: "metrics",
                exchange = %fill.exchange,
                symbol = %fill.symbol,
                side = %fill.side,
                price = %fill.price,
                size = %fill.size,
                fee = ?fill.fee,
                reason = ?fill.reason,
                ts = fill.timestamp,
                "metrics.recent_fill"
            );
        }
        prom
    }

    /// 某腿的持仓读数（账本还没发过该所读数，或该所读数里没有这个币时为 `None`）
    fn leg_value(&self, (symbol, exchange): &Leg) -> Option<&LegValue> {
        self.positions.get(exchange)?.get(symbol)
    }

    /// 某腿本次会话的存货估值变化：(仓位 − 会话起始仓位) × 估值价。还没有估值价（启动后该所第一条
    /// 盘口之前，且空仓）时为 0
    fn session_inventory_delta(&self, leg: &Leg) -> f64 {
        let base = self.baseline.get(leg).copied().unwrap_or(Quantity::ZERO);
        self.leg_value(leg)
            .and_then(|v| v.mark().map(|m| to_f64((v.size() - base) * m)))
            .unwrap_or(0.0)
    }

    /// 本轮全部指标（同时写结构化日志）
    fn render(&self) -> PromText {
        let mut prom = PromText::default();
        // ---------- 账户：净值、账本持仓价值（按所、按币种，不相加） ----------
        let mut account_notional: BTreeMap<(Exchange, &str), f64> = BTreeMap::new();
        for (leg, quote) in &self.quotes {
            if let Some(v) = self.leg_value(leg) {
                *account_notional.entry((leg.1, quote.as_str())).or_default() += v.value();
            }
        }
        for (exchange, equity) in &self.equities {
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_equity",
                &[("exchange", &ex_label), ("asset", &equity.asset)],
                equity.amount,
            );
            tracing::info!(target: "metrics", %exchange, equity = format!("{:.2}", equity.amount), asset = %equity.asset, "metrics.equity");
        }
        for ((exchange, asset), notional) in &account_notional {
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_account_notional",
                &[("exchange", &ex_label), ("asset", asset)],
                *notional,
            );
            tracing::info!(target: "metrics", %exchange, notional = format!("{notional:.2}"), %asset, "metrics.account_notional");
        }

        // ---------- 每条腿：持仓、持仓价值、会话盈亏 ----------
        // 汇总只按 (所, 计价币) 分组：同一币种同一所的数才相加
        let mut session_pnl: BTreeMap<(Exchange, &str), f64> = BTreeMap::new();
        let mut volume: BTreeMap<(Exchange, &str), f64> = BTreeMap::new();
        let mut fees: BTreeMap<&str, f64> = BTreeMap::new();
        let (mut fills, mut forced_fills, mut rejected_orders) = (0u64, 0u64, 0u64);
        let mut legs: Vec<(&Leg, &String)> = self.quotes.iter().collect();
        legs.sort();
        for (leg, quote) in legs {
            let (symbol, exchange) = leg;
            let stats = self.per_leg.get(leg);
            let value = self.leg_value(leg).filter(|v| !v.size().is_zero());
            let delta = self.session_inventory_delta(leg);
            let pnl = stats.map_or(delta, |s| s.total_pnl(delta));
            *session_pnl.entry((*exchange, quote)).or_default() += pnl;
            if let Some(s) = stats {
                *volume.entry((*exchange, quote)).or_default() += s.volume;
                for (asset, amount) in &s.fees {
                    *fees.entry(asset).or_default() += amount;
                }
                fills += s.fills;
                forced_fills += s.forced_fills;
                rejected_orders += s.rejected_orders;
            }
            // 只输出有持仓或本次会话交易过的腿，避免上百个空腿刷屏
            if value.is_none() && stats.is_none() {
                continue;
            }
            let ex_label = exchange.to_string();
            let labels = [("symbol", symbol.as_str()), ("exchange", ex_label.as_str())];
            let money_labels = [
                ("symbol", symbol.as_str()),
                ("exchange", ex_label.as_str()),
                ("asset", quote.as_str()),
            ];
            if let Some(v) = value {
                prom.gauge("hft_position_size", &labels, to_f64(v.size()));
                prom.gauge("hft_position_notional", &money_labels, v.signed_value());
            }
            prom.gauge("hft_leg_session_pnl", &money_labels, pnl);
            prom.gauge(
                "hft_leg_fills",
                &labels,
                stats.map_or(0.0, |s| s.fills as f64),
            );
            tracing::info!(
                target: "metrics",
                %symbol,
                %exchange,
                size = %value.map_or(Quantity::ZERO, |v| v.size()),
                notional = format!("{:.2}", value.map_or(0.0, |v| v.signed_value())),
                fills = stats.map_or(0, |s| s.fills),
                fees = ?stats.map(|s| &s.fees),
                session_pnl = format!("{pnl:.4}"),
                %quote,
                "metrics.leg"
            );
        }
        for ((exchange, asset), pnl) in &session_pnl {
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_session_pnl",
                &[("exchange", &ex_label), ("asset", asset)],
                *pnl,
            );
        }
        for ((exchange, asset), v) in &volume {
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_volume",
                &[("exchange", &ex_label), ("asset", asset)],
                *v,
            );
        }
        // 手续费按币种分开推（#817）：同一币种跨所可以相加，不同币不能
        for (asset, amount) in &fees {
            prom.gauge("hft_fee_total", &[("asset", asset)], *amount);
        }

        // ---------- 订单 ----------
        let mut pending_orders = 0usize;
        let mut pending: Vec<(&Symbol, usize)> = self
            .open_orders
            .iter()
            .map(|(s, o)| (s, o.len()))
            .filter(|(_, n)| *n > 0)
            .collect();
        pending.sort();
        for (symbol, n) in pending {
            pending_orders += n;
            prom.gauge("hft_pending_orders", &[("symbol", symbol)], n as f64);
        }
        prom.gauge("hft_pending_orders_total", &[], pending_orders as f64);
        prom.gauge("hft_rejected_orders_total", &[], rejected_orders as f64);
        prom.gauge("hft_fills_total", &[], fills as f64);
        prom.gauge("hft_forced_fills_total", &[], forced_fills as f64);
        // 恒为 1，标签带提交号：看板据此显示在跑的版本，换版前后能对上（#742）
        prom.gauge(
            "hft_build_info",
            &[("commit", crate::build_info::GIT_COMMIT)],
            1.0,
        );
        tracing::info!(
            target: "metrics",
            exchanges = self.equities.len(),
            ?session_pnl,
            ?volume,
            ?fees,
            pending_orders,
            tracked_legs = self.quotes.len(),
            untracked_events = self.untracked_events,
            rejected_orders,
            fills,
            forced_fills,
            "metrics.summary"
        );

        // ---------- 资金费（实盘，过去 24h，按结算 ID 去重；按 币/所/币种 分开，不相加） ----------
        let mut funding: BTreeMap<(&Symbol, Exchange, &str), f64> = BTreeMap::new();
        for ((exchange, _), (symbol, amount, _)) in &self.funding_fees {
            *funding
                .entry((symbol, *exchange, amount.asset.as_str()))
                .or_default() += amount.amount;
        }
        for ((symbol, exchange, asset), amount) in &funding {
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_funding_fee_24h",
                &[
                    ("symbol", symbol.as_str()),
                    ("exchange", ex_label.as_str()),
                    ("asset", asset),
                ],
                *amount,
            );
        }
        tracing::info!(target: "metrics", funding_fee_24h = ?funding, "metrics.funding");

        // ---------- 执行管线 ----------
        // 下单 / 撤单请求累计次数（本进程启动以来，只增不减；Grafana 用 increase 看 24h 用量，进程重启归零它能识别）
        for (exchange, kind, n) in self.order_requests.snapshot() {
            prom.gauge(
                "hft_order_requests",
                &[("exchange", &exchange.to_string()), ("kind", kind.label())],
                n as f64,
            );
        }
        // 本周期策略消费事件的最大滞后，按账户
        let mut lags: Vec<_> = self.event_lag_max_ms.iter().collect();
        lags.sort_by_key(|(a, _)| a.to_string());
        for (account, ms) in lags {
            prom.gauge(
                "hft_event_lag_max_ms",
                &[("account", &account.to_string())],
                *ms as f64,
            );
        }

        // ---------- 策略自报读数 ----------
        // 逐行把关（见 Gauge::validate），名字加标签重复的也丢：pushgateway 整份收或整份拒
        let mut seen: HashSet<String> = HashSet::new();
        for (account, gauges) in self.strategy_gauges.values() {
            let account_label = account.to_string();
            for g in gauges {
                let verdict = g.validate().and_then(|()| {
                    let key = format!("{}{:?}{account_label}", g.name, g.labels);
                    seen.insert(key)
                        .then_some(())
                        .ok_or_else(|| format!("{} 重复: {:?}", g.name, g.labels))
                });
                match verdict {
                    Ok(()) => prom.push(g, &[("account", account_label.as_str())]),
                    Err(e) => {
                        tracing::warn!(target: "metrics", error = %e, "策略读数不合格，本轮不推")
                    }
                }
            }
        }

        // ---------- 模拟账户 ----------
        for (account, stats) in &self.paper_stats {
            let account_label = account.to_string();
            prom.gauge(
                "hft_paper_fills",
                &[("account", &account_label)],
                stats.fills as f64,
            );
            prom.gauge(
                "hft_paper_volume",
                &[("account", &account_label)],
                stats.volume,
            );
            for (asset, amount) in &stats.fees {
                prom.gauge(
                    "hft_paper_fee",
                    &[("account", &account_label), ("asset", asset)],
                    *amount,
                );
            }
            prom.gauge("hft_paper_cash", &[("account", &account_label)], stats.cash);
            prom.gauge(
                "hft_paper_rejected_orders",
                &[("account", &account_label)],
                stats.rejected_orders as f64,
            );
            tracing::info!(
                target: "metrics",
                %account,
                fills = stats.fills,
                volume = format!("{:.2}", stats.volume),
                fees = ?stats.fees,
                cash = format!("{:.4}", stats.cash),
                rejected_orders = stats.rejected_orders,
                "metrics.paper_account"
            );
        }
        for ((account, exchange), equity) in &self.paper_equities {
            let account_label = account.to_string();
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_paper_equity",
                &[
                    ("account", &account_label),
                    ("exchange", &ex_label),
                    ("asset", &equity.asset),
                ],
                equity.amount,
            );
            tracing::info!(
                target: "metrics",
                %account,
                %exchange,
                equity = format!("{:.2}", equity.amount),
                asset = %equity.asset,
                "metrics.paper_equity"
            );
        }

        prom
    }
}

/// Metrics 的收件箱消息
enum MetricsMsg {
    /// 账户总线（全量订阅）
    Account(AccountEvent),
    /// 见 [`MetricsHandle::strategy_gauges`]
    StrategyGauges {
        executor: ExecutorId,
        account: AccountId,
        gauges: Vec<Gauge>,
    },
    /// 见 [`MetricsHandle::event_lag`]
    EventLag { account: AccountId, max_lag_ms: u64 },
    /// 见 [`MetricsHandle::register_legs`]
    Register {
        metas: Vec<SymbolMeta>,
        baselines: Vec<PositionBaseline>,
        reply: oneshot::Sender<()>,
    },
}

/// Metrics 的对外句柄（可克隆；投产编排用它注册 symbol）
#[derive(Clone, Debug)]
pub struct MetricsHandle(Addr<MetricsMsg>);

impl MetricsHandle {
    /// 一个策略实例交来它最新的自报读数（不等处理；整份替换上一份，空 = 实例已撤下）。
    /// `Err(Gone)` = Metrics 已停止收件。
    pub fn strategy_gauges(
        &self,
        executor: ExecutorId,
        account: AccountId,
        gauges: Vec<Gauge>,
    ) -> Result<(), Gone> {
        self.0.send(MetricsMsg::StrategyGauges {
            executor,
            account,
            gauges,
        })
    }

    /// 策略实例报告一拍（一个 Clock 周期）内消费事件的最大滞后（事件被适配层收到到被策略
    /// 处理之间的毫秒数）。指标按账户取上报周期内的最大值推出
    pub fn event_lag(&self, account: AccountId, max_lag_ms: u64) -> Result<(), Gone> {
        self.0.send(MetricsMsg::EventLag {
            account,
            max_lag_ms,
        })
    }

    /// 投产注册握手：要跟踪的腿（每对 (所, 币) 的合约规格，取其计价币给金额标币种）+ 它们的
    /// 持仓基线（同一份快照，与 executor / 对账镜像口径一致）。注册与基线**原子**到达 —— 不存在
    /// "已注册、基线未到"的窗口，也就不需要缓冲重放机制。
    ///
    /// **等注册完成**才返回（确认完成后才放行事件流，否则盈亏基线可能晚于第一笔成交）。
    /// `Err(Gone)` = Metrics 已停止收件（停机途中或已出错退出）。
    pub async fn register_legs(
        &self,
        metas: Vec<SymbolMeta>,
        baselines: Vec<PositionBaseline>,
    ) -> Result<(), Gone> {
        self.0
            .ask(|reply| MetricsMsg::Register {
                metas,
                baselines,
                reply,
            })
            .await
    }
}

/// 交易指标观测组件。
///
/// 观测组件故障**不得**拖垮交易：`run` 没有出错退出的路径（出错会经任务树让整机停机），
/// pushgateway 推送由子任务 [`Pusher`] 做，失败只记 warn。
pub struct Metrics {
    inbox: Inbox<MetricsMsg>,
    /// 报告间隔
    every: Duration,
    view: MetricsView,
    /// 推送器地址（未配置 pushgateway 为 None）
    pusher: Option<Addr<String>>,
}

impl Metrics {
    /// 在 `group` 下起指标观测，等它订阅完成，返回句柄。订阅见 [`MetricsArgs`] 各字段。
    pub async fn spawn(group: &Group, args: MetricsArgs) -> anyhow::Result<MetricsHandle> {
        let (_, addr) = group
            .spawn_addressed(
                "Metrics",
                move |scope: Scope, mut inbox: Inbox<MetricsMsg>| async move {
                    let pusher = match args.push {
                        Some(config) => Some(Pusher::spawn(&scope.group("push"), config).await?),
                        None => None,
                    };
                    inbox.subscribe(&args.account_bus, |_| true, MetricsMsg::Account);
                    tracing::info!(interval_ms = args.interval_ms, "Metrics started");
                    Ok(Self {
                        inbox,
                        every: Duration::from_millis(args.interval_ms),
                        view: MetricsView::new(args.order_requests),
                        pusher,
                    })
                },
            )
            .await?;
        Ok(MetricsHandle(addr))
    }

    /// 输出一次报告；配了推送就把同一份数字交给推送器
    fn report(&mut self) {
        let prom = self.view.report();
        if let Some(pusher) = &self.pusher {
            // 只可能是推送器自己出错退出（已由任务树上报）
            if pusher.send(prom.into_body()).is_err() {
                tracing::warn!("指标推送器已退出，本次快照未推送");
            }
        }
    }

    /// 测试用：在 `group` 下起一个只写日志、不订任何真实总线的 Metrics
    #[cfg(test)]
    pub(crate) async fn spawn_for_test(group: &Group) -> MetricsHandle {
        Self::spawn(
            group,
            MetricsArgs {
                interval_ms: DEFAULT_REPORT_INTERVAL_MS,
                push: None,
                account_bus: AccountBus::new("account"),
                order_requests: Default::default(),
            },
        )
        .await
        .expect("Metrics 应能起来")
    }
}

impl Task for Metrics {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(self.every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = self.inbox.recv() => match msg {
                    Some(MetricsMsg::Account(ev)) => self.view.on_account(ev),
                    Some(MetricsMsg::StrategyGauges { executor, account, gauges }) => {
                        self.view.set_strategy_gauges(executor, account, gauges)
                    }
                    Some(MetricsMsg::EventLag { account, max_lag_ms }) => self.view.record_event_lag(account, max_lag_ms),
                    Some(MetricsMsg::Register { metas, baselines, reply }) => {
                        self.view.register(metas, baselines);
                        // 提问方已不等回复（它自己放弃了）时无人可告知；注册本身已生效
                        let _ = reply.send(());
                    }
                    // 已被要求停止，收件箱已排空：留一份最终快照（在此而非 stop：见 Task::stop）
                    None => {
                        self.report();
                        tracing::info!("Metrics 最终快照已出（配了推送则已交推送器）");
                        return Ok(());
                    }
                },
                _ = tick.tick() => self.report(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fill, FillReason, Position, Side};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    /// 投产握手的基线载荷（snapshot_req_ts=0；需要累加的 Fill 应带 local_ts > 0）
    fn baseline_of(symbol: &str, size: Decimal) -> crate::messaging::PositionBaseline {
        crate::messaging::PositionBaseline {
            position: Position {
                exchange: Exchange::Binance,
                symbol: symbol.to_string(),
                size,
            },
            snapshot_req_ts: 0,
        }
    }

    /// 一条腿的合约规格（只有计价币要紧）：HL 计价 USDC，其余 USDT
    fn meta(exchange: Exchange, symbol: &str) -> SymbolMeta {
        SymbolMeta {
            exchange,
            symbol: symbol.to_string(),
            price_rule: Arc::new(crate::domain::PriceStep::new(
                crate::domain::PositiveDecimal::ONE,
            )),
            size_step: crate::domain::PositiveDecimal::ONE,
            min_order_size: crate::domain::PositiveDecimal::ONE,
            min_notional: 0.0,
            contract_size: crate::domain::PositiveDecimal::ONE,
            quote: if exchange == Exchange::Hyperliquid {
                "USDC"
            } else {
                "USDT"
            }
            .to_string(),
        }
    }

    /// 在三所都跟踪这些币
    fn legs(symbols: &[&str]) -> Vec<SymbolMeta> {
        symbols
            .iter()
            .flat_map(|s| {
                [Exchange::Binance, Exchange::OKX, Exchange::Hyperliquid].map(|ex| meta(ex, s))
            })
            .collect()
    }

    fn fill_data(exchange: Exchange, symbol: &str, side: Side, size: Decimal) -> AccountData {
        AccountData::Symbol(SymbolAccountData::Fill(Fill {
            exchange,
            symbol: symbol.to_string(),
            side,
            price: dec!(100.0),
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: 0,
            fee: None,
            reason: FillReason::Normal,
        }))
    }

    /// 直接构造聚合视图（不起组件），用于测试纯状态累积逻辑
    fn view() -> MetricsView {
        MetricsView::new(Default::default())
    }

    /// 注册经句柄等到完成；组件停止后句柄报 Gone（调用方据此报错，而不是静默丢掉基线）
    #[tokio::test]
    async fn register_through_handle_then_gone_after_stop() {
        let root = crate::tasks::Root::new("t");
        let handle = Metrics::spawn(
            &root.group("g"),
            MetricsArgs {
                interval_ms: DEFAULT_REPORT_INTERVAL_MS,
                push: None,
                account_bus: AccountBus::new("account"),
                order_requests: Default::default(),
            },
        )
        .await
        .unwrap();
        let register = || (legs(&["BTC"]), vec![baseline_of("BTC", dec!(1.0))]);
        let (metas, baselines) = register();
        handle
            .register_legs(metas, baselines)
            .await
            .expect("注册应完成");
        root.shutdown().await.unwrap();
        let (metas, baselines) = register();
        assert!(
            handle.register_legs(metas, baselines).await.is_err(),
            "停止后应报 Gone"
        );
    }

    /// 假 pushgateway：每个请求先拖 `delay` 再回 200，回完才记下请求体
    async fn slow_gateway(
        delay: Duration,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = got.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let record = record.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // 读到头部结束且正文收满为止
                    let body = loop {
                        let n = sock.read(&mut chunk).await.unwrap();
                        assert!(n > 0, "请求没读完连接就断了");
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let len: usize = text[..end]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            if buf.len() >= end + 4 + len {
                                break text[end + 4..].to_string();
                            }
                        }
                    };
                    tokio::time::sleep(delay).await;
                    sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                    record.lock().unwrap().push(body);
                });
            }
        });
        (url, got)
    }

    /// 停机时最后一份快照推完才算停完（#712：以前是游离 spawn，进程一退就可能被砍断）
    #[tokio::test]
    async fn final_snapshot_is_pushed_before_shutdown_returns() {
        let (url, got) = slow_gateway(Duration::from_millis(300)).await;
        let account_bus = AccountBus::new("account");
        let root = crate::tasks::Root::new("t");
        Metrics::spawn(
            &root.group("g"),
            MetricsArgs {
                interval_ms: DEFAULT_REPORT_INTERVAL_MS,
                push: Some(MetricsPushConfig {
                    url,
                    job: "t".to_string(),
                }),
                account_bus: account_bus.clone(),
                order_requests: Default::default(),
            },
        )
        .await
        .unwrap();
        // 等首拍那份推完（首拍立即触发，那时还没有任何净值）
        tokio::time::timeout(Duration::from_secs(5), async {
            while got.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("首拍快照没推出来");
        assert!(!got.lock().unwrap()[0].contains("hft_equity"));

        account_bus.publish(AccountEvent {
            account: AccountId::Live,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Exchange(ExchangeAccountData::Equity {
                exchange: Exchange::OKX,
                equity: Money::new(4321.0, "USDT"),
            }),
        });
        root.shutdown().await.unwrap();

        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2, "停机返回时最终快照应已推完: {got:?}");
        assert!(
            got[1].contains("hft_equity{exchange=\"OKX\",asset=\"USDT\"} 4321"),
            "最终快照要含停机前收到的净值: {}",
            got[1]
        );
    }

    fn live(data: AccountData) -> AccountEvent {
        AccountEvent {
            account: AccountId::Live,
            exchange_ts: 1,
            local_ts: 1,
            data,
        }
    }

    fn funding_fee(
        exchange: Exchange,
        tran_id: u64,
        amount: f64,
        timestamp: Timestamp,
    ) -> AccountEvent {
        let asset = if exchange == Exchange::Hyperliquid {
            "USDC"
        } else {
            "USDT"
        };
        live(AccountData::Symbol(SymbolAccountData::FundingFee(
            crate::domain::FundingFee {
                exchange,
                symbol: "BTC".to_string(),
                amount: Money::new(amount, asset),
                timestamp,
                tran_id,
            },
        )))
    }

    fn line_value(body: &str, prefix: &str) -> Option<f64> {
        body.lines()
            .find(|l| l.starts_with(prefix))
            .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
    }

    /// 轮询器每分钟重发最近 24h：同一结算 ID 只算一次；出窗的被清掉。按 币/所/币种 推，
    /// 不推跨所合计（USDT 与 USDC 不相加，#841）
    #[test]
    fn funding_fees_dedup_by_settlement_and_roll_off_after_24h() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), Vec::new());
        let now = 100 * FUNDING_FEE_WINDOW_MS;
        for _ in 0..2 {
            metrics.on_account(funding_fee(Exchange::Binance, 1, 2.0, now - 1000));
            metrics.on_account(funding_fee(
                Exchange::Binance,
                2,
                -0.5,
                now - FUNDING_FEE_WINDOW_MS - 1,
            ));
            metrics.on_account(funding_fee(Exchange::Hyperliquid, 1, 3.0, now - 1000));
        }
        metrics.prune_funding_fees(now);
        let body = metrics.render().into_body();
        let v = |k: &str| line_value(&body, k);
        assert_eq!(
            v("hft_funding_fee_24h{symbol=\"BTC\",exchange=\"Binance\",asset=\"USDT\"}"),
            Some(2.0),
            "{body}"
        );
        assert_eq!(
            v("hft_funding_fee_24h{symbol=\"BTC\",exchange=\"Hyperliquid\",asset=\"USDC\"}"),
            Some(3.0),
            "{body}"
        );
        assert!(!body.contains("hft_funding_fee_24h_total"), "{body}");
    }

    /// 策略读数带上实例账户标签推出；实例撤下（交来空的一份）后不再出现
    #[test]
    fn strategy_gauges_follow_their_executor() {
        let mut metrics = view();
        let id = ExecutorId::for_test(7);
        metrics.set_strategy_gauges(
            id,
            AccountId::Live,
            vec![Gauge::new("x_apr", &[("symbol", "BTC")], 0.5)],
        );
        let body = metrics.render().into_body();
        assert!(
            body.contains("x_apr{symbol=\"BTC\",account=\"live\"} 0.5"),
            "{body}"
        );
        metrics.set_strategy_gauges(id, AccountId::Live, Vec::new());
        assert!(!metrics.render().into_body().contains("x_apr"));
    }

    /// 不合格的策略读数（占框架前缀、名字非法、重复）丢掉，其余照推
    #[test]
    fn bad_strategy_gauges_are_dropped_not_pushed() {
        let mut metrics = view();
        let g = |name: &str, sym: &str| Gauge::new(name, &[("symbol", sym)], 1.0);
        metrics.set_strategy_gauges(
            ExecutorId::for_test(1),
            AccountId::Live,
            vec![
                g("hft_equity", "A"),
                g("bad-name", "A"),
                g("ok_x", "A"),
                g("ok_x", "A"),
                g("ok_x", "B"),
            ],
        );
        let body = metrics.render().into_body();
        assert_eq!(
            body.lines().filter(|l| l.starts_with("ok_x")).count(),
            2,
            "{body}"
        );
        assert!(
            !body.contains("bad-name") && !body.contains("hft_equity{symbol"),
            "{body}"
        );
    }

    /// 下单 / 撤单请求计数按所按种类推出；事件滞后按账户取周期内最大值，报完清零（#757）
    #[test]
    fn order_requests_and_event_lag_are_reported() {
        let counts = Arc::new(OrderRequestCounts::default());
        let mut metrics = MetricsView::new(counts.clone());
        counts.bump_for_test(Exchange::Hyperliquid, super::super::RequestKind::Place, 3);
        counts.bump_for_test(Exchange::Hyperliquid, super::super::RequestKind::Cancel, 2);
        metrics.record_event_lag(AccountId::Live, 40);
        metrics.record_event_lag(AccountId::Live, 900);
        metrics.record_event_lag(AccountId::Live, 10);
        let body = metrics.report().into_body();
        assert_eq!(
            line_value(
                &body,
                "hft_order_requests{exchange=\"Hyperliquid\",kind=\"place\"}"
            ),
            Some(3.0),
            "{body}"
        );
        assert_eq!(
            line_value(
                &body,
                "hft_order_requests{exchange=\"Hyperliquid\",kind=\"cancel\"}"
            ),
            Some(2.0),
            "{body}"
        );
        assert_eq!(
            line_value(&body, "hft_event_lag_max_ms{account=\"live\"}"),
            Some(900.0),
            "{body}"
        );
        let next = metrics.report().into_body();
        assert!(
            !next.contains("hft_event_lag_max_ms"),
            "报过的周期应清零: {next}"
        );
    }

    /// 持仓账本发来的读数（实盘）
    fn readings(exchange: Exchange, legs: &[(&str, Decimal, Option<Decimal>)]) -> AccountEvent {
        live(AccountData::Exchange(ExchangeAccountData::PositionValues(
            PositionValues {
                exchange,
                legs: legs
                    .iter()
                    .map(|&(s, size, mark)| (s.to_string(), LegValue::new(size, mark).unwrap()))
                    .collect(),
            },
        )))
    }

    fn equity(exchange: Exchange, amount: f64, asset: &str) -> AccountEvent {
        live(AccountData::Exchange(ExchangeAccountData::Equity {
            exchange,
            equity: Money::new(amount, asset),
        }))
    }

    /// 各所分腿持仓按所推出、持仓价值带计价币，数取自账本读数；不推跨所净敞口、单腿（那是策略的口径）
    #[test]
    fn per_exchange_legs_come_from_ledger_readings() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), Vec::new());
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(1), Some(dec!(100)))],
        ));
        metrics.on_account(readings(
            Exchange::Hyperliquid,
            &[("BTC", dec!(-1), Some(dec!(101)))],
        ));
        // 成交不再改看板上的持仓（那是账本的事）
        metrics.on_account(live(fill_data(
            Exchange::Hyperliquid,
            "BTC",
            Side::Short,
            dec!(5),
        )));
        let body = metrics.render().into_body();
        let v = |k: &str| line_value(&body, k);
        assert_eq!(
            v("hft_position_size{symbol=\"BTC\",exchange=\"Binance\"}"),
            Some(1.0),
            "{body}"
        );
        assert_eq!(
            v("hft_position_size{symbol=\"BTC\",exchange=\"Hyperliquid\"}"),
            Some(-1.0),
            "{body}"
        );
        assert_eq!(
            v("hft_position_notional{symbol=\"BTC\",exchange=\"Hyperliquid\",asset=\"USDC\"}"),
            Some(-101.0),
            "{body}"
        );
        for gone in ["net_notional", "single_leg", "gross_notional", "net_size"] {
            assert!(!body.contains(gone), "{gone}: {body}");
        }
    }

    /// 净值与账本持仓价值按所、按币种推出；不推杠杆和跨所合计（策略的口径，#841）
    #[test]
    fn account_facts_are_per_exchange_and_currency() {
        let mut metrics = view();
        metrics.register(legs(&["BTC", "ETH"]), Vec::new());
        metrics.on_account(equity(Exchange::OKX, 1000.0, "USDT"));
        metrics.on_account(equity(Exchange::Hyperliquid, 2000.0, "USDC"));
        metrics.on_account(readings(
            Exchange::OKX,
            &[
                ("BTC", dec!(-2), Some(dec!(100))),
                ("ETH", dec!(3), Some(dec!(100))),
            ],
        ));
        let body = metrics.render().into_body();
        let v = |k: &str| line_value(&body, k);
        assert_eq!(
            v("hft_equity{exchange=\"OKX\",asset=\"USDT\"}"),
            Some(1000.0),
            "{body}"
        );
        assert_eq!(
            v("hft_equity{exchange=\"Hyperliquid\",asset=\"USDC\"}"),
            Some(2000.0),
            "{body}"
        );
        assert_eq!(
            v("hft_account_notional{exchange=\"OKX\",asset=\"USDT\"}"),
            Some(500.0),
            "{body}"
        );
        for gone in [
            "hft_total_equity",
            "hft_account_leverage",
            "hft_total_account_notional",
        ] {
            assert!(!body.contains(gone), "{gone}: {body}");
        }
        // 账本还没发过读数的所：持仓价值未知，不推（不是 0）
        assert!(
            !body.contains("hft_account_notional{exchange=\"Hyperliquid\""),
            "{body}"
        );
    }

    /// 模拟账户事件按账户分账，**绝不**混进实盘视图
    #[test]
    fn paper_events_never_leak_into_live_view() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), Vec::new());
        let account = AccountId::Paper("BTC".to_string());
        metrics.on_account(AccountEvent {
            account: account.clone(),
            ..readings(Exchange::Binance, &[("BTC", dec!(1), Some(dec!(100)))])
        });
        metrics.on_account(AccountEvent {
            account: account.clone(),
            ..live(fill_data(Exchange::Binance, "BTC", Side::Long, dec!(1.0)))
        });
        assert_eq!(metrics.paper_stats[&account].fills, 1);
        assert!(metrics.per_leg.is_empty(), "模拟成交不得计入实盘累计");
        assert!(metrics.positions.is_empty(), "模拟读数不得进实盘持仓");
    }

    /// 没注册的腿（别的币，或注册过的币在没注册的所）的事件只计数、不计入
    #[test]
    fn untracked_leg_events_are_counted_not_applied() {
        let mut metrics = view();
        metrics.register(vec![meta(Exchange::Binance, "BTC")], Vec::new());
        metrics.on_account(live(fill_data(
            Exchange::Binance,
            "ETH",
            Side::Long,
            dec!(1.0),
        )));
        metrics.on_account(live(fill_data(Exchange::OKX, "BTC", Side::Long, dec!(1.0))));
        assert_eq!(metrics.untracked_events, 2);
        assert!(metrics.per_leg.is_empty());
    }

    /// 启动时已有 2 个多头（重启场景）、本次会话没成交：会话盈亏为 0，而非凭空 +存货市值
    #[test]
    fn session_pnl_excludes_pre_existing_inventory() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), vec![baseline_of("BTC", dec!(2.0))]);
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(2), Some(dec!(100)))],
        ));
        let body = metrics.render().into_body();
        assert_eq!(
            line_value(
                &body,
                "hft_session_pnl{exchange=\"Binance\",asset=\"USDT\"}"
            ),
            Some(0.0),
            "{body}"
        );
    }

    /// 本次会话又买 1 个（价 100），账本报现价 110 → 存货变化 +110，现金流 -100 → 浮盈 10；
    /// 按所、按计价币分开（HL 的 USDC 盈亏不并进币安的 USDT）
    #[test]
    fn session_pnl_tracks_this_session_trades_per_exchange_and_currency() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), vec![baseline_of("BTC", dec!(2.0))]);
        metrics.on_account(live(fill_data(
            Exchange::Binance,
            "BTC",
            Side::Long,
            dec!(1.0),
        )));
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(3), Some(dec!(110)))],
        ));
        // HL 卖 1 个（价 100），现价 110 → 现金 +100，存货 -110 → -10
        metrics.on_account(live(fill_data(
            Exchange::Hyperliquid,
            "BTC",
            Side::Short,
            dec!(1.0),
        )));
        metrics.on_account(readings(
            Exchange::Hyperliquid,
            &[("BTC", dec!(-1), Some(dec!(110)))],
        ));
        let body = metrics.render().into_body();
        let v = |k: &str| line_value(&body, k).unwrap();
        assert!(
            (v("hft_session_pnl{exchange=\"Binance\",asset=\"USDT\"}") - 10.0).abs() < 1e-9,
            "{body}"
        );
        assert!(
            (v("hft_session_pnl{exchange=\"Hyperliquid\",asset=\"USDC\"}") + 10.0).abs() < 1e-9,
            "{body}"
        );
        assert!(
            (v("hft_leg_session_pnl{symbol=\"BTC\",exchange=\"Binance\",asset=\"USDT\"}") - 10.0)
                .abs()
                < 1e-9,
            "{body}"
        );
        assert!(
            (v("hft_volume{exchange=\"Hyperliquid\",asset=\"USDC\"}") - 100.0).abs() < 1e-9,
            "{body}"
        );
    }

    /// 平掉的腿（仓位回到 0）照样推会话盈亏，按所的合计里也还在
    #[test]
    fn closed_leg_keeps_its_session_pnl() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), Vec::new());
        let mut buy = fill_data(Exchange::Binance, "BTC", Side::Long, dec!(1.0));
        metrics.on_account(live(buy.clone()));
        if let AccountData::Symbol(SymbolAccountData::Fill(f)) = &mut buy {
            f.side = Side::Short;
            f.price = dec!(103);
        }
        metrics.on_account(live(buy));
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(0), Some(dec!(103)))],
        ));
        let body = metrics.render().into_body();
        assert_eq!(
            line_value(
                &body,
                "hft_leg_session_pnl{symbol=\"BTC\",exchange=\"Binance\",asset=\"USDT\"}"
            ),
            Some(3.0),
            "{body}"
        );
        assert_eq!(
            line_value(
                &body,
                "hft_session_pnl{exchange=\"Binance\",asset=\"USDT\"}"
            ),
            Some(3.0),
            "{body}"
        );
        assert!(!body.contains("hft_position_size"), "空仓不推持仓: {body}");
    }

    /// 手续费按币种分开推，不同币不相加；没收手续费的成交不出现币种（#817）
    #[test]
    fn fees_are_pushed_per_asset() {
        let mut metrics = view();
        metrics.register(legs(&["BTC"]), vec![baseline_of("BTC", dec!(0))]);
        let with_fee = |exchange, fee: Option<Money>| {
            let mut data = fill_data(exchange, "BTC", Side::Long, dec!(1.0));
            if let AccountData::Symbol(SymbolAccountData::Fill(f)) = &mut data {
                f.fee = fee;
            }
            live(data)
        };
        let fee = Money::new;
        metrics.on_account(with_fee(Exchange::Binance, Some(fee(0.02, "USDT"))));
        metrics.on_account(with_fee(Exchange::OKX, Some(fee(0.01, "USDT"))));
        metrics.on_account(with_fee(Exchange::Binance, Some(fee(0.0001, "BNB"))));
        metrics.on_account(with_fee(Exchange::Hyperliquid, Some(fee(0.03, "USDC"))));
        metrics.on_account(with_fee(Exchange::OKX, None));
        let body = metrics.render().into_body();
        let get = |asset: &str| line_value(&body, &format!("hft_fee_total{{asset=\"{asset}\"}} "));
        assert!((get("USDT").unwrap() - 0.03).abs() < 1e-12, "{body}");
        assert_eq!(get("BNB"), Some(0.0001), "{body}");
        assert_eq!(get("USDC"), Some(0.03), "{body}");
        assert_eq!(body.matches("hft_fee_total{").count(), 3, "{body}");
    }
}

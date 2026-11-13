//! Metrics - 交易指标观测
//!
//! 订阅账户总线的**全量**事件流，聚合后定时输出结构化日志：
//! - **账户**：各交易所 equity / 持仓价值 / 账户杠杆率
//! - **持仓**：各 symbol 在各交易所的仓位、净敞口、名义价值
//! - **订单**：当前挂单数、累计拒单数
//! - **历史**：累计成交笔数/成交额/手续费、现金流盈亏、强平次数、最近成交明细
//!
//! 设计要点：
//! - **持仓与持仓价值只读持仓账本发的读数**（[`PositionValues`]，#812）：看板上的持仓、杠杆与
//!   策略风控闸门看到的是同一个数，本模块不自己折叠成交、不自己估值。账户杠杆 = 账本给的本所
//!   持仓价值之和 ÷ 交易所报的净值，与策略的账户杠杆闸门同一算法
//! - **不读取任何 executor 的内部状态**，与策略层零耦合
//! - 盈亏为**本次会话**口径：现金流 + 相对会话起始仓位的存货估值变化，
//!   这样重启后不会把重启前就持有的存货算成假盈利
//! - 默认只输出日志；配置 `PUSHGATEWAY_URL` 后同一份数字推送 pushgateway
//!   （见 [`crate::observability`]，未配置时无外部依赖）

use super::{ExecutorId, OrderRequestCounts};
use crate::domain::{
    now_ms, to_f64, AccountId, Exchange, Quantity, Symbol, Timestamp, TradingStats,
};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, LegValue, PositionBaseline,
    PositionValues, RequestOutcome, SymbolAccountData,
};
use crate::observability::{Gauge, MetricsPushConfig, PromText, Pusher};
use crate::tasks::{Addr, Gone, Group, Inbox, Scope, Task};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

/// 默认指标报告间隔
pub const DEFAULT_REPORT_INTERVAL_MS: u64 = 60_000;

/// 资金费收入的统计窗口：过去 24h（交易所的资费轮询器每次也是重发最近 24h，靠结算 ID 去重）
const FUNDING_FEE_WINDOW_MS: u64 = 24 * 3600 * 1000;

/// per-symbol 统计不保留逐笔明细，明细只在全局那一份上保留
const NO_PER_SYMBOL_FILL_DETAIL: usize = 0;

/// 单 symbol 跨所持仓与估值汇总 —— **观测口径，只属于本模块**。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SymbolExposure {
    /// 各交易所带符号仓位之和（净敞口，币本位）
    pub net_size: Quantity,
    /// Σ |仓位| × 估值价（总名义敞口）
    pub gross_notional: f64,
    /// Σ 仓位 × 估值价（净名义敞口，理想对冲下应接近 0）
    pub net_notional: f64,
    /// 相对**基线仓位**的名义变化：Σ (仓位 − 基线) × 估值价
    ///
    /// 基线 = 本次会话开始时的既有仓位。用于把"重启前就持有的存货"从盈亏里剔除，
    /// 否则 `cash`（本次会话从 0 起算）加上全额存货市值，会凭空多出一笔等于存货市值的假盈利。
    pub session_notional_delta: f64,
    /// 有非零仓位的交易所个数
    pub legs: usize,
}

/// 汇总一个 symbol 的跨所持仓与估值（读数来自持仓账本）。
///
/// `baseline` 为各所的会话起始仓位（缺失视为 0）。账本的读数保证有仓必有估值价；空仓又还没有
/// 盘口的腿（只可能在启动后、该所第一条盘口之前）不计入会话变化。
fn exposure_of(
    legs: &[(Exchange, LegValue)],
    baseline: Option<&HashMap<Exchange, Quantity>>,
) -> SymbolExposure {
    let mut exposure = SymbolExposure::default();
    for (exchange, leg) in legs {
        let base = baseline
            .and_then(|b| b.get(exchange))
            .copied()
            .unwrap_or(Quantity::ZERO);
        if let Some(mark) = leg.mark() {
            exposure.session_notional_delta += to_f64((leg.size() - base) * mark);
        }
        if leg.size().is_zero() {
            continue;
        }
        exposure.legs += 1;
        exposure.net_size += leg.size();
        exposure.gross_notional += leg.value();
        exposure.net_notional += leg.signed_value();
    }
    exposure
}

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
    /// 各所交易所报的净值（实盘 `AccountInfo`）
    equities: HashMap<Exchange, f64>,
    /// 持仓账本发的最新持仓读数：所 -> 币 -> 读数（实盘 `PositionValues`，见模块文档）
    positions: HashMap<Exchange, HashMap<Symbol, LegValue>>,
    /// 交易所说还开着的本引擎挂单：symbol -> (所, client_order_id)。按订单回报增删（非终态加、终态删）
    open_orders: HashMap<Symbol, HashSet<(Exchange, String)>>,
    /// 正在跟踪的 symbol（由 [`MetricsHandle::register_symbols`] 注册）
    tracked: HashSet<Symbol>,
    /// 会话起始仓位基线：symbol -> (exchange -> 仓位)
    ///
    /// 取自投产握手 [`MetricsHandle::register_symbols`] 携带的基线（每个 (symbol, 所) 只记第一条）。
    /// 用于把重启前的存货从盈亏里剔除。
    baseline: HashMap<Symbol, HashMap<Exchange, Quantity>>,
    /// per-symbol 累计成交统计
    per_symbol: HashMap<Symbol, TradingStats>,
    /// 全账户累计成交统计
    total: TradingStats,
    /// 落在跟踪范围外、被跳过的 symbol 事件数
    ///
    /// **分桶部署下这个数持续增长是正常的**：多实例共用同一账户时，私有流会把其他桶的
    /// symbol 事件也推给本实例。若为单实例全量部署且此数持续增长，才说明注册范围有问题。
    untracked_events: u64,
    /// 模拟账户累计成交统计（来自账户总线上带 Paper 标签的事件），与实盘视图分账
    paper_stats: HashMap<AccountId, TradingStats>,
    /// 各模拟账户柜台的最新净值：(账户, 所) -> (equity, notional)。
    /// 已知取舍：账户撤下后条目残留（= 最后一次快照，持续上报），生命周期内不清理
    paper_accounts: HashMap<(AccountId, Exchange), (f64, f64)>,
    /// 各策略实例最新一份自报读数（见 [`crate::strategy::Strategy::gauges`]），按实例归属；
    /// 实例撤下时交来空的一份，条目即删除
    strategy_gauges: HashMap<ExecutorId, (AccountId, Vec<Gauge>)>,
    /// 实盘账户收到 / 付出的资金费：(所, 结算 ID) -> (币, 金额, 结算时刻)。
    /// 交易所轮询器每次重发最近 24h，同一笔按结算 ID 只记一次；超出窗口的在报告时清掉
    funding_fees: HashMap<(Exchange, u64), (Symbol, f64, Timestamp)>,
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
            tracked: HashSet::new(),
            baseline: HashMap::new(),
            per_symbol: HashMap::new(),
            total: TradingStats::default(),
            untracked_events: 0,
            paper_stats: HashMap::new(),
            paper_accounts: HashMap::new(),
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

    /// 投产注册：跟踪范围 + 会话起始仓位基线（见 [`MetricsHandle::register_symbols`]）
    fn register(&mut self, symbols: Vec<Symbol>, baselines: Vec<PositionBaseline>) {
        tracing::info!(count = symbols.len(), "Metrics tracking symbols");
        self.tracked.extend(symbols);
        // 重复注册（同一 symbol 再次投产）时首次值不被覆写
        for b in &baselines {
            self.record_baseline(&b.position.symbol, b.position.exchange, b.position.size);
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
            AccountData::Exchange(ExchangeAccountData::AccountInfo {
                exchange, equity, ..
            }) => {
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
        if !self.tracked.contains(data.symbol()) {
            self.untracked_events += 1;
            return;
        }
        match &data {
            SymbolAccountData::Fill(fill) => {
                self.total.apply_fill(fill);
                self.symbol_stats_mut(&fill.symbol).apply_fill(fill);
            }
            SymbolAccountData::FundingFee(fee) => {
                self.funding_fees.insert(
                    (fee.exchange, fee.tran_id),
                    (fee.symbol.clone(), fee.amount, fee.timestamp),
                );
            }
            SymbolAccountData::OrderUpdate(update) => {
                self.total.apply_order_update(update);
                self.symbol_stats_mut(&update.symbol)
                    .apply_order_update(update);
                self.track_open_order(update);
            }
            SymbolAccountData::OrderRequest(r) => {
                if matches!(r.outcome, RequestOutcome::PlaceRejected { .. }) {
                    self.total.apply_place_rejected();
                    self.symbol_stats_mut(&r.symbol).apply_place_rejected();
                }
            }
        }
    }

    fn symbol_stats_mut(&mut self, symbol: &Symbol) -> &mut TradingStats {
        self.per_symbol
            .entry(symbol.clone())
            .or_insert_with(|| TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL))
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

    /// 某币各所的持仓读数（只含账本已发过读数的所）
    fn legs_of(&self, symbol: &Symbol) -> Vec<(Exchange, LegValue)> {
        let mut legs: Vec<(Exchange, LegValue)> = self
            .positions
            .iter()
            .filter_map(|(ex, legs)| legs.get(symbol).map(|leg| (*ex, *leg)))
            .collect();
        legs.sort_by_key(|(ex, _)| ex.to_string());
        legs
    }

    /// 模拟账户事件的分账逻辑（纯状态转移，供 handler 委托与单测直调）。
    /// **绝不**触碰实盘视图（total / positions / equities）。
    fn apply_paper_event(&mut self, account: &AccountId, data: &AccountData) {
        match data {
            AccountData::Symbol(SymbolAccountData::Fill(fill)) => {
                self.paper_stats_mut(account).apply_fill(fill);
            }
            AccountData::Symbol(SymbolAccountData::OrderUpdate(update)) => {
                self.paper_stats_mut(account).apply_order_update(update);
            }
            AccountData::Exchange(ExchangeAccountData::AccountInfo {
                exchange,
                equity,
                notional,
            }) => {
                self.paper_accounts
                    .insert((account.clone(), *exchange), (*equity, *notional));
            }
            _ => {}
        }
    }

    fn paper_stats_mut(&mut self, account: &AccountId) -> &mut TradingStats {
        self.paper_stats
            .entry(account.clone())
            .or_insert_with(|| TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL))
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

    /// 记录会话起始仓位基线（每个 (symbol, exchange) 只记一次）
    fn record_baseline(&mut self, symbol: &Symbol, exchange: Exchange, size: Quantity) {
        self.baseline
            .entry(symbol.clone())
            .or_default()
            .entry(exchange)
            .or_insert(size);
    }

    /// 输出一次完整报告（结构化日志），返回同一份数字的推送文本
    fn report(&mut self) -> PromText {
        self.prune_funding_fees(now_ms());
        let prom = self.render();
        // 滞后按上报周期取最大：这一份已报，下一周期重新累计
        self.event_lag_max_ms.clear();
        if self.total.forced_fills > 0 {
            tracing::warn!(
                target: "metrics",
                forced_fills = self.total.forced_fills,
                "metrics.forced_fills：存在强平/ADL 成交"
            );
        }

        // 最近成交明细放 debug：默认不刷屏，排查时开 RUST_LOG=metrics=debug 即可看到
        for fill in &self.total.recent_fills {
            tracing::debug!(
                target: "metrics",
                exchange = %fill.exchange,
                symbol = %fill.symbol,
                side = %fill.side,
                price = %fill.price,
                size = %fill.size,
                fee = fill.fee,
                reason = ?fill.reason,
                ts = fill.timestamp,
                "metrics.recent_fill"
            );
        }
        prom
    }

    /// 本轮全部指标（同时写结构化日志）
    fn render(&self) -> PromText {
        let mut prom = PromText::default();
        // ---------- 账户 ----------
        let mut total_equity = 0.0;
        let mut total_notional = 0.0;
        let mut exchange_count = 0usize;
        for (exchange, &equity) in &self.equities {
            // 持仓价值 = 账本读数之和（与策略账户杠杆闸门同一个数）；账本还没发过该所读数时未知
            let notional: Option<f64> = self
                .positions
                .get(exchange)
                .map(|legs| legs.values().map(LegValue::value).sum());
            // 净值 ≤0 或持仓价值未知时杠杆算不出：不推这个点（看板断线），不能报成 0 冒充「很安全」
            let leverage = notional.filter(|_| equity > 0.0).map(|n| n / equity);
            total_equity += equity;
            total_notional += notional.unwrap_or(0.0);
            exchange_count += 1;
            let ex_label = exchange.to_string();
            prom.gauge("hft_equity", &[("exchange", &ex_label)], equity);
            if let Some(n) = notional {
                prom.gauge("hft_account_notional", &[("exchange", &ex_label)], n);
            }
            if let Some(l) = leverage {
                prom.gauge("hft_account_leverage", &[("exchange", &ex_label)], l);
            }
            tracing::info!(
                target: "metrics",
                %exchange,
                equity = format!("{:.2}", equity),
                notional = notional.map_or_else(|| "n/a".to_string(), |n| format!("{n:.2}")),
                leverage = leverage.map_or_else(|| "n/a".to_string(), |l| format!("{l:.3}")),
                "metrics.account"
            );
        }

        // ---------- 持仓 ----------
        let mut session_position_value = 0.0;
        let mut gross_notional = 0.0;
        let mut abs_net_notional = 0.0;
        let mut symbols_with_position = 0usize;
        let mut single_leg_symbols = 0usize;
        let mut pending_orders = 0usize;

        for symbol in &self.tracked {
            let legs = self.legs_of(symbol);
            let exposure = exposure_of(&legs, self.baseline.get(symbol));
            let pending = self.open_orders.get(symbol).map_or(0, |o| o.len());
            pending_orders += pending;
            session_position_value += exposure.session_notional_delta;
            gross_notional += exposure.gross_notional;
            abs_net_notional += exposure.net_notional.abs();

            if exposure.legs > 0 {
                symbols_with_position += 1;
                if exposure.legs == 1 {
                    single_leg_symbols += 1;
                }
            }

            // 只输出有持仓或有挂单的 symbol，避免上百个空 symbol 刷屏
            if exposure.legs == 0 && pending == 0 {
                continue;
            }

            let stats = self.per_symbol.get(symbol);
            let sym_label: &str = symbol;
            let session_pnl = stats
                .map(|s| s.total_pnl(exposure.session_notional_delta))
                .unwrap_or(0.0);
            prom.gauge(
                "hft_position_net_size",
                &[("symbol", sym_label)],
                to_f64(exposure.net_size),
            );
            prom.gauge(
                "hft_position_gross_notional",
                &[("symbol", sym_label)],
                exposure.gross_notional,
            );
            prom.gauge(
                "hft_position_net_notional",
                &[("symbol", sym_label)],
                exposure.net_notional,
            );
            prom.gauge(
                "hft_pending_orders",
                &[("symbol", sym_label)],
                pending as f64,
            );
            // 各所分腿：对冲是否到位要按所看（净仓为 0 也可能是两所之间搬错了腿）
            for (exchange, leg) in legs.iter().filter(|(_, leg)| !leg.size().is_zero()) {
                let ex_label = exchange.to_string();
                let labels = [("symbol", sym_label), ("exchange", ex_label.as_str())];
                prom.gauge("hft_position_size", &labels, to_f64(leg.size()));
                prom.gauge("hft_position_notional", &labels, leg.signed_value());
            }
            prom.gauge(
                "hft_symbol_fills",
                &[("symbol", sym_label)],
                stats.map(|s| s.fills as f64).unwrap_or(0.0),
            );
            prom.gauge(
                "hft_symbol_session_pnl",
                &[("symbol", sym_label)],
                session_pnl,
            );
            prom.gauge(
                "hft_single_leg",
                &[("symbol", sym_label)],
                if exposure.legs == 1 { 1.0 } else { 0.0 },
            );
            tracing::info!(
                target: "metrics",
                %symbol,
                legs = exposure.legs,
                net_size = %exposure.net_size,
                gross_notional = format!("{:.2}", exposure.gross_notional),
                net_notional = format!("{:.2}", exposure.net_notional),
                pending_orders = pending,
                fills = stats.map(|s| s.fills).unwrap_or(0),
                fee = format!("{:.4}", stats.map(|s| s.fee).unwrap_or(0.0)),
                session_pnl = format!(
                    "{:.4}",
                    stats
                        .map(|s| s.total_pnl(exposure.session_notional_delta))
                        .unwrap_or(0.0)
                ),
                "metrics.position"
            );
        }

        // ---------- 订单 + 历史 ----------
        let total_session_pnl = self.total.total_pnl(session_position_value);
        prom.gauge("hft_total_equity", &[], total_equity);
        prom.gauge("hft_total_account_notional", &[], total_notional);
        prom.gauge("hft_position_gross_notional_total", &[], gross_notional);
        prom.gauge("hft_position_net_notional_abs_total", &[], abs_net_notional);
        prom.gauge(
            "hft_symbols_with_position",
            &[],
            symbols_with_position as f64,
        );
        prom.gauge("hft_single_leg_symbols", &[], single_leg_symbols as f64);
        prom.gauge("hft_pending_orders_total", &[], pending_orders as f64);
        prom.gauge(
            "hft_rejected_orders_total",
            &[],
            self.total.rejected_orders as f64,
        );
        prom.gauge("hft_fills_total", &[], self.total.fills as f64);
        prom.gauge(
            "hft_forced_fills_total",
            &[],
            self.total.forced_fills as f64,
        );
        prom.gauge("hft_volume_total", &[], self.total.volume);
        prom.gauge("hft_fee_total", &[], self.total.fee);
        prom.gauge("hft_session_pnl", &[], total_session_pnl);
        // 恒为 1，标签带提交号：看板据此显示在跑的版本，换版前后能对上（#742）
        prom.gauge(
            "hft_build_info",
            &[("commit", crate::build_info::GIT_COMMIT)],
            1.0,
        );
        tracing::info!(
            target: "metrics",
            exchanges = exchange_count,
            total_equity = format!("{:.2}", total_equity),
            total_account_notional = format!("{:.2}", total_notional),
            position_gross_notional = format!("{:.2}", gross_notional),
            position_net_notional_abs = format!("{:.2}", abs_net_notional),
            symbols_with_position = symbols_with_position,
            single_leg_symbols = single_leg_symbols,
            pending_orders = pending_orders,
            tracked_symbols = self.tracked.len(),
            untracked_events = self.untracked_events,
            rejected_orders = self.total.rejected_orders,
            fills = self.total.fills,
            forced_fills = self.total.forced_fills,
            volume = format!("{:.2}", self.total.volume),
            fee = format!("{:.4}", self.total.fee),
            cash = format!("{:.4}", self.total.cash),
            session_pnl = format!("{:.4}", total_session_pnl),
            "metrics.summary"
        );

        // ---------- 资金费（实盘，过去 24h，按结算 ID 去重） ----------
        let mut funding: HashMap<(&Symbol, Exchange), f64> = HashMap::new();
        for ((exchange, _), (symbol, amount, _)) in &self.funding_fees {
            *funding.entry((symbol, *exchange)).or_default() += amount;
        }
        let mut funding_total = 0.0;
        for ((symbol, exchange), amount) in &funding {
            funding_total += amount;
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_funding_fee_24h",
                &[("symbol", symbol.as_str()), ("exchange", ex_label.as_str())],
                *amount,
            );
        }
        prom.gauge("hft_funding_fee_24h_total", &[], funding_total);
        tracing::info!(target: "metrics", funding_fee_24h = format!("{:.4}", funding_total), "metrics.funding");

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
            prom.gauge("hft_paper_fee", &[("account", &account_label)], stats.fee);
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
                fee = format!("{:.4}", stats.fee),
                cash = format!("{:.4}", stats.cash),
                rejected_orders = stats.rejected_orders,
                "metrics.paper_account"
            );
        }
        for ((account, exchange), (equity, notional)) in &self.paper_accounts {
            let account_label = account.to_string();
            let ex_label = exchange.to_string();
            prom.gauge(
                "hft_paper_equity",
                &[("account", &account_label), ("exchange", &ex_label)],
                *equity,
            );
            prom.gauge(
                "hft_paper_notional",
                &[("account", &account_label), ("exchange", &ex_label)],
                *notional,
            );
            tracing::info!(
                target: "metrics",
                %account,
                %exchange,
                equity = format!("{:.2}", equity),
                notional = format!("{:.2}", notional),
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
    /// 见 [`MetricsHandle::register_symbols`]
    Register {
        symbols: Vec<Symbol>,
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

    /// 投产注册握手：要跟踪的 symbol 集合 + 它们的持仓基线（同一份快照，与 executor /
    /// 对账镜像口径一致）。注册与基线**原子**到达 —— 不存在"已注册、基线未到"的窗口，
    /// 也就不需要缓冲重放机制。
    ///
    /// **等注册完成**才返回（确认完成后才放行事件流，否则盈亏基线可能晚于第一笔成交）。
    /// `Err(Gone)` = Metrics 已停止收件（停机途中或已出错退出）。
    pub async fn register_symbols(
        &self,
        symbols: Vec<Symbol>,
        baselines: Vec<PositionBaseline>,
    ) -> Result<(), Gone> {
        self.0
            .ask(|reply| MetricsMsg::Register {
                symbols,
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
                    Some(MetricsMsg::Register { symbols, baselines, reply }) => {
                        self.view.register(symbols, baselines);
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
            fee: 0.0,
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
        let register = || (vec!["BTC".to_string()], vec![baseline_of("BTC", dec!(1.0))]);
        let (symbols, baselines) = register();
        handle
            .register_symbols(symbols, baselines)
            .await
            .expect("注册应完成");
        root.shutdown().await.unwrap();
        let (symbols, baselines) = register();
        assert!(
            handle.register_symbols(symbols, baselines).await.is_err(),
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
            data: AccountData::Exchange(ExchangeAccountData::AccountInfo {
                exchange: Exchange::OKX,
                equity: 4321.0,
                notional: 0.0,
            }),
        });
        root.shutdown().await.unwrap();

        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2, "停机返回时最终快照应已推完: {got:?}");
        assert!(
            got[1].contains("hft_equity{exchange=\"OKX\"} 4321"),
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
        live(AccountData::Symbol(SymbolAccountData::FundingFee(
            crate::domain::FundingFee {
                exchange,
                symbol: "BTC".to_string(),
                asset: "USDT".to_string(),
                amount,
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

    /// 轮询器每分钟重发最近 24h：同一结算 ID 只算一次；出窗的被清掉
    #[test]
    fn funding_fees_dedup_by_settlement_and_roll_off_after_24h() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], Vec::new());
        let now = 100 * FUNDING_FEE_WINDOW_MS;
        for _ in 0..2 {
            metrics.on_account(funding_fee(Exchange::Binance, 1, 2.0, now - 1000));
            metrics.on_account(funding_fee(
                Exchange::Binance,
                2,
                -0.5,
                now - FUNDING_FEE_WINDOW_MS - 1,
            ));
            metrics.on_account(funding_fee(Exchange::OKX, 1, 3.0, now - 1000));
        }
        metrics.prune_funding_fees(now);
        let body = metrics.render().into_body();
        assert_eq!(
            line_value(
                &body,
                "hft_funding_fee_24h{symbol=\"BTC\",exchange=\"Binance\"}"
            ),
            Some(2.0),
            "{body}"
        );
        assert_eq!(
            line_value(
                &body,
                "hft_funding_fee_24h{symbol=\"BTC\",exchange=\"OKX\"}"
            ),
            Some(3.0),
            "{body}"
        );
        assert_eq!(
            line_value(&body, "hft_funding_fee_24h_total "),
            Some(5.0),
            "{body}"
        );
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

    fn equity(exchange: Exchange, equity: f64) -> AccountEvent {
        live(AccountData::Exchange(ExchangeAccountData::AccountInfo {
            exchange,
            equity,
            // 交易所报的持仓总值不进看板（看板只读账本读数，#812）
            notional: 1e9,
        }))
    }

    /// 各所分腿持仓按所推出（净仓为 0 时也看得到两条腿），数取自账本读数
    #[test]
    fn per_exchange_legs_come_from_ledger_readings() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], Vec::new());
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(1), Some(dec!(100)))],
        ));
        metrics.on_account(readings(
            Exchange::OKX,
            &[("BTC", dec!(-1), Some(dec!(101)))],
        ));
        // 成交不再改看板上的持仓（那是账本的事）
        metrics.on_account(live(fill_data(Exchange::OKX, "BTC", Side::Short, dec!(5))));
        let body = metrics.render().into_body();
        let v = |k: &str| line_value(&body, k);
        assert_eq!(
            v("hft_position_size{symbol=\"BTC\",exchange=\"Binance\"}"),
            Some(1.0),
            "{body}"
        );
        assert_eq!(
            v("hft_position_size{symbol=\"BTC\",exchange=\"OKX\"}"),
            Some(-1.0),
            "{body}"
        );
        assert_eq!(
            v("hft_position_notional{symbol=\"BTC\",exchange=\"OKX\"}"),
            Some(-101.0),
            "{body}"
        );
        assert_eq!(
            v("hft_position_net_notional{symbol=\"BTC\"}"),
            Some(-1.0),
            "{body}"
        );
    }

    /// 账户杠杆 = 账本读数之和 ÷ 净值（与策略账户杠杆闸门同一个数），不用交易所报的持仓总值
    #[test]
    fn account_leverage_uses_ledger_readings() {
        let mut metrics = view();
        metrics.on_account(equity(Exchange::OKX, 1000.0));
        metrics.on_account(readings(
            Exchange::OKX,
            &[
                ("BTC", dec!(-2), Some(dec!(100))),
                ("ETH", dec!(3), Some(dec!(100))),
            ],
        ));
        let body = metrics.render().into_body();
        assert_eq!(
            line_value(&body, "hft_account_notional{exchange=\"OKX\"}"),
            Some(500.0),
            "{body}"
        );
        assert_eq!(
            line_value(&body, "hft_account_leverage{exchange=\"OKX\"}"),
            Some(0.5),
            "{body}"
        );
    }

    /// 净值 ≤0、或账本还没发过该所读数时杠杆算不出：不推杠杆点（看板断线），不得报成 0 冒充「很安全」
    #[test]
    fn leverage_not_reported_when_unknown() {
        let mut metrics = view();
        metrics.on_account(equity(Exchange::Binance, 0.0));
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(1), Some(dec!(100)))],
        ));
        metrics.on_account(equity(Exchange::OKX, 1000.0));
        let body = metrics.render().into_body();
        assert!(!body.contains("hft_account_leverage"), "{body}");
        assert!(
            !body.contains("hft_account_notional{exchange=\"OKX\"}"),
            "{body}"
        );
    }

    /// 模拟账户事件按账户分账，**绝不**混进实盘视图
    #[test]
    fn paper_events_never_leak_into_live_view() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], Vec::new());
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
        assert_eq!(metrics.total.fills, 0, "模拟成交不得计入实盘累计");
        assert!(metrics.positions.is_empty(), "模拟读数不得进实盘持仓");
    }

    #[test]
    fn baseline_records_only_the_first_position_snapshot() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], vec![baseline_of("BTC", dec!(2.0))]);
        metrics.register(vec!["BTC".to_string()], vec![baseline_of("BTC", dec!(9.0))]);
        assert_eq!(
            metrics.baseline["BTC"][&Exchange::Binance],
            dec!(2.0),
            "基线只取第一次快照"
        );
    }

    #[test]
    fn untracked_symbol_events_are_counted_not_applied() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], Vec::new());
        metrics.on_account(live(fill_data(
            Exchange::Binance,
            "ETH",
            Side::Long,
            dec!(1.0),
        )));
        assert_eq!(metrics.untracked_events, 1);
        assert_eq!(metrics.total.fills, 0);
    }

    /// 启动时已有 2 个多头（重启场景）、本次会话没成交：会话盈亏为 0，而非凭空 +存货市值
    #[test]
    fn session_pnl_excludes_pre_existing_inventory() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], vec![baseline_of("BTC", dec!(2.0))]);
        metrics.on_account(readings(
            Exchange::Binance,
            &[("BTC", dec!(2), Some(dec!(100)))],
        ));
        let body = metrics.render().into_body();
        assert_eq!(
            line_value(&body, "hft_position_net_notional{symbol=\"BTC\"}"),
            Some(200.0)
        );
        assert_eq!(line_value(&body, "hft_session_pnl "), Some(0.0), "{body}");
    }

    /// 本次会话又买 1 个（价 100），账本报现价 110 → 存货变化 +110，现金流 -100 → 浮盈 10
    #[test]
    fn session_pnl_tracks_this_session_trades() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], vec![baseline_of("BTC", dec!(2.0))]);
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
        let body = metrics.render().into_body();
        let pnl = line_value(&body, "hft_session_pnl ").unwrap();
        assert!((pnl - 10.0).abs() < 1e-9, "{body}");
    }

    // ===== 跨所估值 =====

    fn leg(size: Decimal, mark: Option<Decimal>) -> LegValue {
        LegValue::new(size, mark).unwrap()
    }

    #[test]
    fn hedged_position_has_near_zero_net_notional() {
        let legs = [
            (Exchange::Binance, leg(dec!(1), Some(dec!(100)))),
            (Exchange::OKX, leg(dec!(-1), Some(dec!(100)))),
        ];
        let exposure = exposure_of(&legs, None);
        assert_eq!(exposure.legs, 2);
        assert!(exposure.net_notional.abs() < 1e-9);
        assert!((exposure.gross_notional - 200.0).abs() < 1e-9);
    }

    #[test]
    fn baseline_position_is_excluded_from_session_delta() {
        let legs = [(Exchange::Binance, leg(dec!(3), Some(dec!(100))))];
        let baseline = HashMap::from([(Exchange::Binance, dec!(2.0))]);
        let exposure = exposure_of(&legs, Some(&baseline));
        // 全额估值 300，但本次会话只新增了 1 个 → 100
        assert!((exposure.net_notional - 300.0).abs() < 1e-9);
        assert!((exposure.session_notional_delta - 100.0).abs() < 1e-9);
    }

    /// 平掉了启动时就有的仓：会话变化按平仓后的估值价算（空仓腿也带价）
    #[test]
    fn closing_baseline_inventory_counts_in_session_delta() {
        let legs = [(Exchange::Binance, leg(dec!(0), Some(dec!(100))))];
        let baseline = HashMap::from([(Exchange::Binance, dec!(2.0))]);
        let exposure = exposure_of(&legs, Some(&baseline));
        assert_eq!(exposure.legs, 0);
        assert!((exposure.session_notional_delta + 200.0).abs() < 1e-9);
    }

    #[test]
    fn missing_baseline_defaults_to_zero() {
        let legs = [(Exchange::Binance, leg(dec!(3), Some(dec!(100))))];
        let exposure = exposure_of(&legs, Some(&HashMap::new()));
        assert!((exposure.session_notional_delta - 300.0).abs() < 1e-9);
    }

    #[test]
    fn zero_positions_are_ignored() {
        let legs = [(Exchange::Binance, leg(dec!(0), None))];
        assert_eq!(exposure_of(&legs, None), SymbolExposure::default());
    }
}

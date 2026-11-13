//! Metrics - 交易指标观测
//!
//! 订阅行情与账户两条总线的**全量**事件流，自建一份聚合视图并定时输出结构化日志：
//! - **账户**：各交易所 equity / 持仓名义 / 账户杠杆率
//! - **持仓**：各 symbol 在各交易所的仓位、净敞口、名义价值
//! - **订单**：当前挂单数、累计拒单数
//! - **历史**：累计成交笔数/成交额/手续费、现金流盈亏、强平次数、最近成交明细
//!
//! 设计要点：
//! - 视图完全由事件流重建，**不读取任何 executor 的内部状态**，与策略层零耦合
//! - 持仓/挂单维护直接复用 [`StateManager`]（策略用的同一份实现），不重复实现
//! - 跨所持仓估值由本模块的 [`SymbolExposure`] 算，累计统计复用 [`TradingStats`]
//! - 盈亏为**本次会话**口径：现金流 + 相对会话起始仓位的存货估值变化，
//!   这样 docker 重启后不会把重启前就持有的存货算成假盈利
//! - 默认只输出日志；配置 `PUSHGATEWAY_URL` 后同一份数字推送 pushgateway
//!   （见 [`crate::observability`]，未配置时无外部依赖、行为与从前一致）

use super::{ExecutorId, OrderRequestCounts};
use crate::domain::{
    now_ms, to_f64, AccountId, Exchange, Quantity, Symbol, Timestamp, TradingStats,
};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, IncomeEvent, MarketBus,
    MarketEvent, PositionBaseline, RequestOutcome, StateManager, SymbolAccountData, SymbolState,
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
///
/// # 为什么住在这里而不在 `SymbolState` 上
///
/// 它的每个字段都是观测概念：`session_notional_delta` 是"把重启前的存货从盈亏里剔除"，
/// `unpriced_legs` 是"估值不完整的提示"。这些对策略与对账都无意义 —— 领域状态里放一个
/// 只有指标层看得懂的汇总，就是观测口径泄漏进领域层（`docs/architecture.md` V7 / P3）。
///
/// 它需要的东西 `SymbolState` 都已公开（持仓投影 + 盘口），所以搬过来是纯移动，
/// 领域层不必为观测保留任何接口。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SymbolExposure {
    /// 各交易所带符号仓位之和（净敞口，币本位）
    pub net_size: Quantity,
    /// Σ |仓位| × mid（总名义敞口）
    pub gross_notional: f64,
    /// Σ 仓位 × mid（净名义敞口，理想对冲下应接近 0）
    pub net_notional: f64,
    /// 相对**基线仓位**的名义变化：Σ (仓位 − 基线) × mid
    ///
    /// 基线 = 本次会话开始时的既有仓位。用于把"重启前就持有的存货"从盈亏里剔除，
    /// 否则 `cash`（本次会话从 0 起算）加上全额存货市值，会凭空多出一笔等于存货市值的假盈利。
    pub session_notional_delta: f64,
    /// 有非零仓位的交易所个数
    pub legs: usize,
    /// 有仓位但缺少 BBO、无法估值的交易所个数（估值不完整的提示）
    pub unpriced_legs: usize,
}

/// 汇总一个 symbol 的跨所持仓与估值。
///
/// `baseline` 为各所的会话起始仓位（缺失视为 0）。缺 BBO 的腿不参与估值但计入
/// `unpriced_legs` —— 宁可显式暴露"估值不完整"，也不用兜底价格伪造一个看似完整的数字。
fn exposure_of(
    state: &SymbolState,
    baseline: Option<&HashMap<Exchange, Quantity>>,
) -> SymbolExposure {
    let mut exposure = SymbolExposure::default();
    for position in state.position_book().all() {
        if position.is_empty() {
            continue;
        }
        exposure.legs += 1;
        exposure.net_size += position.size;
        match state.market().bbo(position.exchange) {
            Some(bbo) => {
                let mid = bbo.mid_price();
                let base = baseline
                    .and_then(|b| b.get(&position.exchange))
                    .copied()
                    .unwrap_or(Quantity::ZERO);
                exposure.gross_notional += to_f64(position.size.abs() * mid);
                exposure.net_notional += to_f64(position.size * mid);
                exposure.session_notional_delta += to_f64((position.size - base) * mid);
            }
            None => exposure.unpriced_legs += 1,
        }
    }
    exposure
}

/// Metrics 构造参数
pub struct MetricsArgs {
    /// 报告间隔 (毫秒)
    pub interval_ms: u64,
    /// 可选的 Prometheus pushgateway 推送（None = 只输出日志，与从前一致）
    pub push: Option<MetricsPushConfig>,
    /// 订阅：全量行情（持仓估值 / 市场状态视图）
    pub market_bus: MarketBus,
    /// 订阅：全量账户事件（实盘 + 全部模拟账户一次订阅，按事件自带的账户分账）
    pub account_bus: AccountBus,
    /// 实盘网关的下单 / 撤单请求计数（网关写、这里只读，见 [`OrderRequestCounts`]）
    pub order_requests: Arc<OrderRequestCounts>,
}

/// 从事件流重建的聚合视图（纯状态，不含收件与定时；单测直接构造）
struct MetricsView {
    /// 账户 / 持仓视图（复用策略侧同一实现）。订单跟踪表不用它的：那份只认本进程策略发出的单，
    /// 观测这里看不到策略发单，见 [`Self::open_orders`]
    state: StateManager,
    /// 交易所说还开着的本引擎挂单：symbol -> (所, client_order_id)。按订单回报增删（非终态加、终态删）
    open_orders: HashMap<Symbol, HashSet<(Exchange, String)>>,
    /// 正在跟踪的 symbol（由 [`MetricsHandle::register_symbols`] 注册）
    tracked: HashSet<Symbol>,
    /// 会话起始仓位基线：symbol -> (exchange -> 仓位)
    ///
    /// 取自投产握手 [`MetricsHandle::register_symbols`] 携带的基线（每个 (symbol, 所) 只记第一条，
    /// 与 `SymbolState` 的 seed 语义一致）。用于把重启前的存货从盈亏里剔除。
    baseline: HashMap<Symbol, HashMap<Exchange, Quantity>>,
    /// per-symbol 累计成交统计
    per_symbol: HashMap<Symbol, TradingStats>,
    /// 全账户累计成交统计
    total: TradingStats,
    /// 落在跟踪范围外、被跳过的 symbol 事件数
    ///
    /// **分桶部署下这个数持续增长是正常的**：多实例共用同一账户时，私有流会把其他桶的
    /// symbol 事件也推给本实例。它的用途是"本实例的跟踪范围与事件流是否对得上"的参考量，
    /// 不是错误计数。若为单实例全量部署且此数持续增长，才说明注册范围有问题。
    untracked_events: u64,
    /// 模拟账户累计成交统计（来自账户总线上带 Paper 标签的事件）。
    ///
    /// **与实盘视图分账**：模拟事件绝不喂进 `state`（StateManager 按 exchange 键控，
    /// 柜台的 AccountInfo 会覆盖实盘净值、Fill 会污染实盘持仓镜像）。此前模拟账户的
    /// 成交/拒单/净值完全不进指标，是观测盲区。
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
            state: StateManager::new(&[]),
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
        self.state.register_symbols(&symbols);
        self.tracked.extend(symbols);
        // 会话起始仓位基线（盈亏口径）与持仓镜像各记一份；重复注册（同一 symbol 再次投产）由
        // record_baseline 的 or_insert 与 seed 的幂等各自兜住，首次值不被覆写
        for b in &baselines {
            self.record_baseline(&b.position.symbol, b.position.exchange, b.position.size);
        }
        self.state.seed_positions(&baselines);
    }

    /// 行情事件入口（持仓估值 / 市场状态视图）
    fn on_market(&mut self, msg: MarketEvent) {
        // 跟踪范围外的 symbol 事件直接计数跳过：喂给 StateManager 会被当成"路由 bug"打 error，
        // 但对观测层这只是"不关心的 symbol"（分桶部署下必然出现），不是错误。
        if let Some(symbol) = msg.data.symbol() {
            if !self.tracked.contains(symbol) {
                self.untracked_events += 1;
                return;
            }
        }
        self.apply_state(IncomeEvent::Market(msg));
    }

    /// 账户事件入口：实盘进实盘视图（state/total），模拟账户按账户分账 —— 分道由事件
    /// 自带的账户标签决定，不依赖"来源总线"区分
    fn on_account(&mut self, msg: AccountEvent) {
        // 穷举而非 `!= Live`：新增账户类型时这里编译失败（原则 P6）。用否定条件的话
        // 新类型会被静默并进模拟账本 —— 观测口径出错且没有任何症状。
        //
        // 这里**不复用** `outlet_for`：那是"订单发往哪个执行出口"，与"记进哪本账"是
        // 两件事，恰好同形不等于同一个概念，合并会是假抽象。
        match msg.account {
            AccountId::Paper(_) => {
                self.apply_paper_event(&msg.account.clone(), &msg.data);
                return;
            }
            AccountId::Live => {}
        }
        if let Some(symbol) = msg.data.symbol() {
            if !self.tracked.contains(symbol) {
                self.untracked_events += 1;
                return;
            }
        }
        // 累计统计
        match &msg.data {
            AccountData::Symbol(SymbolAccountData::Fill(fill)) => {
                self.total.apply_fill(fill);
                self.per_symbol
                    .entry(fill.symbol.clone())
                    .or_insert_with(|| {
                        TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL)
                    })
                    .apply_fill(fill);
            }
            AccountData::Symbol(SymbolAccountData::FundingFee(fee)) => {
                self.funding_fees.insert(
                    (fee.exchange, fee.tran_id),
                    (fee.symbol.clone(), fee.amount, fee.timestamp),
                );
            }
            AccountData::Symbol(SymbolAccountData::OrderUpdate(update)) => {
                self.total.apply_order_update(update);
                self.per_symbol
                    .entry(update.symbol.clone())
                    .or_insert_with(|| {
                        TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL)
                    })
                    .apply_order_update(update);
                self.track_open_order(update);
                return; // 订单回报不进 state（见 state 字段说明）
            }
            AccountData::Symbol(SymbolAccountData::OrderRequest(r)) => {
                if matches!(r.outcome, RequestOutcome::PlaceRejected { .. }) {
                    self.total.apply_place_rejected();
                    self.per_symbol
                        .entry(r.symbol.clone())
                        .or_insert_with(|| {
                            TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL)
                        })
                        .apply_place_rejected();
                }
                return;
            }
            _ => {}
        }
        // 账户 / 持仓视图（观测侧没有订单跟踪表，state 里不会有单，不会报单子卡住）
        self.apply_state(IncomeEvent::Account(msg));
    }

    /// 喂进 state。观测视图从不登记订单，「有单等交易所太久」的报错不会发生；真出现只记 error
    fn apply_state(&mut self, event: IncomeEvent) {
        if let Err(e) = self.state.apply(&event) {
            tracing::error!(error = %e, "观测视图的订单跟踪报错（不该发生：这里不登记订单）");
        }
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
    /// **绝不**触碰实盘视图（total / state）—— 串账防线，见 paper_stats 字段说明。
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
        for (exchange, info) in self.state.account_view().account_infos() {
            // 净值 ≤0 时杠杆算不出：不推这个点（看板断线），不能报成 0 冒充「很安全」
            let leverage = (info.equity > 0.0).then(|| info.notional / info.equity);
            total_equity += info.equity;
            total_notional += info.notional;
            exchange_count += 1;
            let ex_label = exchange.to_string();
            prom.gauge("hft_equity", &[("exchange", &ex_label)], info.equity);
            prom.gauge(
                "hft_account_notional",
                &[("exchange", &ex_label)],
                info.notional,
            );
            if let Some(l) = leverage {
                prom.gauge("hft_account_leverage", &[("exchange", &ex_label)], l);
            }
            tracing::info!(
                target: "metrics",
                %exchange,
                equity = format!("{:.2}", info.equity),
                notional = format!("{:.2}", info.notional),
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
        let mut unpriced_legs = 0usize;
        let mut pending_orders = 0usize;

        for (symbol, symbol_state) in self.state.symbol_states() {
            let exposure = exposure_of(symbol_state, self.baseline.get(symbol));
            let pending = self.open_orders.get(symbol).map_or(0, |o| o.len());
            pending_orders += pending;
            session_position_value += exposure.session_notional_delta;
            gross_notional += exposure.gross_notional;
            abs_net_notional += exposure.net_notional.abs();
            unpriced_legs += exposure.unpriced_legs;

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
            for position in symbol_state.position_book().all() {
                if position.is_empty() {
                    continue;
                }
                let ex_label = position.exchange.to_string();
                let labels = [("symbol", sym_label), ("exchange", ex_label.as_str())];
                prom.gauge("hft_position_size", &labels, to_f64(position.size));
                if let Some(bbo) = symbol_state.market().bbo(position.exchange) {
                    prom.gauge(
                        "hft_position_notional",
                        &labels,
                        to_f64(position.size * bbo.mid_price()),
                    );
                }
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
                unpriced_legs = exposure.unpriced_legs,
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
        prom.gauge("hft_unpriced_legs", &[], unpriced_legs as f64);
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
            unpriced_legs = unpriced_legs,
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
    /// 行情总线（全量订阅）
    Market(MarketEvent),
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
                    inbox.subscribe(&args.market_bus, |_| true, MetricsMsg::Market);
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
                market_bus: MarketBus::new("market"),
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
                    Some(MetricsMsg::Market(ev)) => self.view.on_market(ev),
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
                market_bus: MarketBus::new("market"),
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
                market_bus: MarketBus::new("market"),
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

    /// 各所分腿持仓按所推出（净仓为 0 时也看得到两条腿）
    #[test]
    fn per_exchange_legs_are_reported() {
        let mut metrics = view();
        metrics.register(vec!["BTC".to_string()], Vec::new());
        metrics.on_account(live(fill_data(
            Exchange::Binance,
            "BTC",
            Side::Long,
            dec!(1.0),
        )));
        metrics.on_account(live(fill_data(
            Exchange::OKX,
            "BTC",
            Side::Short,
            dec!(1.0),
        )));
        let body = metrics.render().into_body();
        assert_eq!(
            line_value(
                &body,
                "hft_position_size{symbol=\"BTC\",exchange=\"Binance\"}"
            ),
            Some(1.0),
            "{body}"
        );
        assert_eq!(
            line_value(&body, "hft_position_size{symbol=\"BTC\",exchange=\"OKX\"}"),
            Some(-1.0),
            "{body}"
        );
    }

    /// 净值 ≤0 时杠杆算不出：不推杠杆点（看板断线），不得报成 0 冒充「很安全」
    #[test]
    fn leverage_not_reported_when_equity_non_positive() {
        let mut metrics = view();
        for (exchange, equity) in [(Exchange::Binance, 0.0), (Exchange::OKX, 1000.0)] {
            metrics
                .state
                .apply(&IncomeEvent::account(
                    AccountId::Live,
                    1,
                    1,
                    AccountData::Exchange(ExchangeAccountData::AccountInfo {
                        exchange,
                        equity,
                        notional: 500.0,
                    }),
                ))
                .unwrap();
        }
        let body = metrics.render().into_body();
        let leverage: Vec<&str> = body
            .lines()
            .filter(|l| l.starts_with("hft_account_leverage"))
            .collect();
        assert_eq!(leverage.len(), 1, "只有净值为正的所有杠杆点: {leverage:?}");
        assert!(
            leverage[0].contains(&Exchange::OKX.to_string()) && leverage[0].ends_with(" 0.5"),
            "{leverage:?}"
        );
    }

    /// 模拟账户事件按账户分账，**绝不**混进实盘视图（total/state）
    #[tokio::test]
    async fn paper_events_never_leak_into_live_view() {
        let mut metrics = view();
        metrics.tracked.insert("BTC".to_string());
        metrics.state.register_symbols(&["BTC".to_string()]);

        let account = crate::domain::AccountId::Paper("BTC".to_string());
        let data = fill_data(Exchange::Binance, "BTC", Side::Long, dec!(1.0));
        // 调 handler 委托的真实分账方法：将来有人在里面误加 state.apply，本测试会红
        metrics.apply_paper_event(&account, &data);

        assert_eq!(metrics.paper_stats[&account].fills, 1);
        assert_eq!(metrics.total.fills, 0, "模拟成交不得计入实盘累计");
        assert_eq!(
            metrics
                .state
                .symbol_state(&"BTC".to_string())
                .unwrap()
                .position_book()
                .position_size(Exchange::Binance),
            dec!(0.0),
            "模拟成交不得进实盘持仓镜像"
        );
    }

    #[tokio::test]
    async fn baseline_records_only_the_first_position_snapshot() {
        let mut metrics = view();
        metrics.tracked.insert("BTC".to_string());
        metrics.state.register_symbols(&["BTC".to_string()]);

        // 启动快照 2.0，随后又来一条 Position（不应覆盖基线）
        metrics.record_baseline(&"BTC".to_string(), Exchange::Binance, dec!(2.0));
        metrics.record_baseline(&"BTC".to_string(), Exchange::Binance, dec!(9.0));

        assert_eq!(
            metrics.baseline["BTC"][&Exchange::Binance],
            dec!(2.0),
            "基线只取第一次快照"
        );
    }

    #[tokio::test]
    async fn untracked_symbol_events_are_counted_not_applied() {
        let mut metrics = view();
        metrics.tracked.insert("BTC".to_string());
        metrics.state.register_symbols(&["BTC".to_string()]);

        // 走组件真正用的账户事件入口
        metrics.on_account(AccountEvent {
            account: AccountId::Live,
            exchange_ts: 1,
            local_ts: 1,
            data: fill_data(Exchange::Binance, "ETH", Side::Long, dec!(1.0)),
        });

        assert_eq!(metrics.untracked_events, 1);
        assert_eq!(metrics.total.fills, 0);
    }

    #[tokio::test]
    async fn session_pnl_excludes_pre_existing_inventory() {
        let mut metrics = view();
        let symbol = "BTC".to_string();
        metrics.tracked.insert(symbol.clone());
        metrics
            .state
            .register_symbols(std::slice::from_ref(&symbol));

        // 启动时已有 2 个多头（重启场景）：投产握手 seed 基线
        metrics.record_baseline(&symbol, Exchange::Binance, dec!(2.0));
        metrics
            .state
            .seed_positions(&[baseline_of(&symbol, dec!(2.0))]);
        // 喂 BBO 让持仓可估值
        metrics
            .state
            .apply(&IncomeEvent::market(
                0,
                0,
                crate::messaging::MarketData::BBO(crate::domain::BBO {
                    exchange: Exchange::Binance,
                    symbol: symbol.clone(),
                    bid_price: dec!(100.0),
                    bid_qty: dec!(1.0),
                    ask_price: dec!(100.0),
                    ask_qty: dec!(1.0),
                    timestamp: 0,
                }),
            ))
            .unwrap();

        let state = metrics.state.symbol_state(&symbol).unwrap();
        let exposure = exposure_of(state, metrics.baseline.get(&symbol));

        // 全额市值 200，但本次会话未成交 → session PnL 为 0，而非凭空 +200
        assert!((exposure.net_notional - 200.0).abs() < 1e-9);
        assert!(exposure.session_notional_delta.abs() < 1e-9);
        assert!(
            metrics
                .total
                .total_pnl(exposure.session_notional_delta)
                .abs()
                < 1e-9
        );
    }

    #[tokio::test]
    async fn session_pnl_tracks_this_session_trades() {
        let mut metrics = view();
        let symbol = "BTC".to_string();
        metrics.tracked.insert(symbol.clone());
        metrics
            .state
            .register_symbols(std::slice::from_ref(&symbol));

        metrics.record_baseline(&symbol, Exchange::Binance, dec!(2.0));
        metrics
            .state
            .seed_positions(&[baseline_of(&symbol, dec!(2.0))]);

        // 本次会话又买 1 个（价 100），随后 BBO 报 110；
        // local_ts 晚于快照请求时刻，不会被 seed 的防双计过滤
        let data = fill_data(Exchange::Binance, &symbol, Side::Long, dec!(1.0));
        if let AccountData::Symbol(SymbolAccountData::Fill(f)) = &data {
            metrics.total.apply_fill(f);
        }
        metrics
            .state
            .apply(&IncomeEvent::account(
                crate::domain::AccountId::Live,
                1,
                1,
                data,
            ))
            .unwrap();
        metrics
            .state
            .apply(&IncomeEvent::market(
                0,
                0,
                crate::messaging::MarketData::BBO(crate::domain::BBO {
                    exchange: Exchange::Binance,
                    symbol: symbol.clone(),
                    bid_price: dec!(110.0),
                    bid_qty: dec!(1.0),
                    ask_price: dec!(110.0),
                    ask_qty: dec!(1.0),
                    timestamp: 0,
                }),
            ))
            .unwrap();

        let state = metrics.state.symbol_state(&symbol).unwrap();
        let exposure = exposure_of(state, metrics.baseline.get(&symbol));

        // 会话内新增 1 个、现价 110 → 存货变化 +110，现金流 -100 → 浮盈 10
        assert!((exposure.session_notional_delta - 110.0).abs() < 1e-9);
        assert!((metrics.total.total_pnl(exposure.session_notional_delta) - 10.0).abs() < 1e-9);
    }

    // ===== 跨所估值（V7 之前住在 SymbolState 上，随口径一起搬来） =====

    const EXPOSURE_SYMBOL: &str = "BTC";

    fn exposure_bbo(exchange: Exchange, mid: Decimal) -> crate::domain::BBO {
        crate::domain::BBO {
            exchange,
            symbol: EXPOSURE_SYMBOL.to_string(),
            bid_price: mid,
            bid_qty: dec!(1.0),
            ask_price: mid,
            ask_qty: dec!(1.0),
            timestamp: 0,
        }
    }

    /// 经**生产路径**构造夹具：仓位走投产 seed、盘口走行情事件。
    /// 直接写字段更省事，但那样测的就不是线上跑的那条路。
    fn state_with(
        positions: &[(Exchange, Decimal)],
        priced: &[(Exchange, Decimal)],
    ) -> SymbolState {
        let mut state = SymbolState::new(EXPOSURE_SYMBOL.to_string());
        for &(exchange, size) in positions {
            state.seed_position(
                &Position {
                    exchange,
                    symbol: EXPOSURE_SYMBOL.to_string(),
                    size,
                },
                0,
            );
        }
        for &(exchange, mid) in priced {
            state.apply_market(&crate::messaging::MarketData::BBO(exposure_bbo(
                exchange, mid,
            )));
        }
        state
    }

    #[test]
    fn hedged_position_has_near_zero_net_notional() {
        let state = state_with(
            &[(Exchange::Binance, dec!(1.0)), (Exchange::OKX, dec!(-1.0))],
            &[
                (Exchange::Binance, dec!(100.0)),
                (Exchange::OKX, dec!(100.0)),
            ],
        );

        let exposure = exposure_of(&state, None);

        assert_eq!(exposure.legs, 2);
        assert!(exposure.net_notional.abs() < 1e-9);
        assert!((exposure.gross_notional - 200.0).abs() < 1e-9);
        assert_eq!(exposure.unpriced_legs, 0);
    }

    #[test]
    fn baseline_position_is_excluded_from_session_delta() {
        let state = state_with(
            &[(Exchange::Binance, dec!(3.0))],
            &[(Exchange::Binance, dec!(100.0))],
        );
        let baseline = HashMap::from([(Exchange::Binance, dec!(2.0))]);

        let exposure = exposure_of(&state, Some(&baseline));

        // 全额估值 300，但本次会话只新增了 1 个 → 100
        assert!((exposure.net_notional - 300.0).abs() < 1e-9);
        assert!((exposure.session_notional_delta - 100.0).abs() < 1e-9);
    }

    #[test]
    fn missing_baseline_defaults_to_zero() {
        let state = state_with(
            &[(Exchange::Binance, dec!(3.0))],
            &[(Exchange::Binance, dec!(100.0))],
        );
        let exposure = exposure_of(&state, Some(&HashMap::new()));
        assert!((exposure.session_notional_delta - 300.0).abs() < 1e-9);
    }

    /// 缺 BBO 的腿显式暴露成 `unpriced_legs`，不用兜底价格伪造一个看似完整的估值
    #[test]
    fn missing_bbo_is_surfaced_not_silently_valued_at_zero() {
        let state = state_with(&[(Exchange::Binance, dec!(1.0))], &[]);

        let exposure = exposure_of(&state, None);

        assert_eq!(exposure.legs, 1);
        assert_eq!(exposure.unpriced_legs, 1);
        assert_eq!(exposure.gross_notional, 0.0);
    }

    #[test]
    fn zero_positions_are_ignored() {
        let state = state_with(
            &[(Exchange::Binance, dec!(0.0))],
            &[(Exchange::Binance, dec!(100.0))],
        );
        assert_eq!(exposure_of(&state, None), SymbolExposure::default());
    }
}

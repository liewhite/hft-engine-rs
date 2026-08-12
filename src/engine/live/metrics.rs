//! MetricsActor - 交易指标观测
//!
//! 订阅 IncomePubSub 的**全量**事件流，自建一份聚合视图并定时输出结构化日志：
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

use crate::domain::{AccountId, Exchange, Symbol, TradingStats};
use crate::messaging::{AccountData, AccountEvent, IncomeEvent, MarketEvent, StateManager, SymbolState};
use crate::observability::{prometheus, MetricsPushConfig, PromText};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;


/// 默认指标报告间隔
pub const DEFAULT_REPORT_INTERVAL_MS: u64 = 60_000;

/// 观测层不做订单超时清理（挂单的生命周期由策略侧负责，这里只如实反映）
const NO_ORDER_TIMEOUT: u64 = 0;

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
    pub net_size: f64,
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
fn exposure_of(state: &SymbolState, baseline: Option<&HashMap<Exchange, f64>>) -> SymbolExposure {
    let mut exposure = SymbolExposure::default();
    for position in state.position_book().all() {
        if position.is_empty() {
            continue;
        }
        exposure.legs += 1;
        exposure.net_size += position.size;
        match state.bbo(position.exchange) {
            Some(bbo) => {
                let mid = bbo.mid_price();
                let base = baseline
                    .and_then(|b| b.get(&position.exchange))
                    .copied()
                    .unwrap_or(0.0);
                exposure.gross_notional += position.size.abs() * mid;
                exposure.net_notional += position.size * mid;
                exposure.session_notional_delta += (position.size - base) * mid;
            }
            None => exposure.unpriced_legs += 1,
        }
    }
    exposure
}

/// MetricsActor 初始化参数
pub struct MetricsActorArgs {
    /// 报告间隔 (毫秒)
    pub interval_ms: u64,
    /// 可选的 Prometheus pushgateway 推送（None = 只输出日志，与从前一致）
    pub push: Option<MetricsPushConfig>,
}

/// MetricsActor - 交易指标观测
pub struct MetricsActor {
    /// 账户 / 持仓 / 挂单视图（复用策略侧同一实现）
    state: StateManager,
    /// 正在跟踪的 symbol（由 [`RegisterSymbols`] 注册）
    tracked: HashSet<Symbol>,
    /// 会话起始仓位基线：symbol -> (exchange -> 仓位)
    ///
    /// 取自投产握手 [`RegisterSymbols`] 携带的基线（每个 (symbol, 所) 只记第一条，
    /// 与 `SymbolState` 的 seed 语义一致）。用于把重启前的存货从盈亏里剔除。
    baseline: HashMap<Symbol, HashMap<Exchange, f64>>,
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
    /// 可选的 pushgateway 推送配置
    push: Option<MetricsPushConfig>,
    /// 模拟账户累计成交统计（来自 PaperPubSub）。
    ///
    /// **与实盘视图分账**：模拟事件绝不喂进 `state`（StateManager 按 exchange 键控，
    /// 柜台的 AccountInfo 会覆盖实盘净值、Fill 会污染实盘持仓镜像）。此前模拟账户的
    /// 成交/拒单/净值完全不进指标，是观测盲区。
    paper_stats: HashMap<AccountId, TradingStats>,
    /// 各模拟账户柜台的最新净值：(账户, 所) -> (equity, notional)。
    /// 已知取舍：账户撤下后条目残留（= 最后一次快照，持续上报），生命周期内不清理
    paper_accounts: HashMap<(AccountId, Exchange), (f64, f64)>,
}

impl MetricsActor {
    /// 模拟账户事件的分账逻辑（纯状态转移，供 handler 委托与单测直调）。
    /// **绝不**触碰实盘视图（total / state）—— 串账防线，见 paper_stats 字段说明。
    fn apply_paper_event(&mut self, account: &AccountId, data: &AccountData) {
        match data {
            AccountData::Fill(fill) => {
                self.paper_stats_mut(account).apply_fill(fill);
            }
            AccountData::OrderUpdate(update) => {
                self.paper_stats_mut(account).apply_order_update(update);
            }
            AccountData::AccountInfo {
                exchange,
                equity,
                notional,
            } => {
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

    /// 记录会话起始仓位基线（每个 (symbol, exchange) 只记一次）
    fn record_baseline(&mut self, symbol: &Symbol, exchange: Exchange, size: f64) {
        self.baseline
            .entry(symbol.clone())
            .or_default()
            .entry(exchange)
            .or_insert(size);
    }

    /// 输出一次完整报告（结构化日志；配置了 pushgateway 时同一份数字同时推送）
    fn report(&self) {
        let mut prom = PromText::default();
        // ---------- 账户 ----------
        let mut total_equity = 0.0;
        let mut total_notional = 0.0;
        let mut exchange_count = 0usize;
        for (exchange, info) in self.state.account_view().account_infos() {
            let leverage = if info.equity > 0.0 {
                info.notional / info.equity
            } else {
                0.0
            };
            total_equity += info.equity;
            total_notional += info.notional;
            exchange_count += 1;
            let ex_label = exchange.to_string();
            prom.gauge("hft_equity", &[("exchange", &ex_label)], info.equity);
            prom.gauge("hft_account_notional", &[("exchange", &ex_label)], info.notional);
            prom.gauge("hft_account_leverage", &[("exchange", &ex_label)], leverage);
            tracing::info!(
                target: "metrics",
                %exchange,
                equity = format!("{:.2}", info.equity),
                notional = format!("{:.2}", info.notional),
                leverage = format!("{:.3}", leverage),
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
            let pending = symbol_state.pending_orders().count();
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
            prom.gauge("hft_position_net_size", &[("symbol", sym_label)], exposure.net_size);
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
            prom.gauge("hft_pending_orders", &[("symbol", sym_label)], pending as f64);
            prom.gauge(
                "hft_symbol_fills",
                &[("symbol", sym_label)],
                stats.map(|s| s.fills as f64).unwrap_or(0.0),
            );
            prom.gauge("hft_symbol_session_pnl", &[("symbol", sym_label)], session_pnl);
            prom.gauge(
                "hft_single_leg",
                &[("symbol", sym_label)],
                if exposure.legs == 1 { 1.0 } else { 0.0 },
            );
            tracing::info!(
                target: "metrics",
                %symbol,
                legs = exposure.legs,
                net_size = format!("{:.6}", exposure.net_size),
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

            // 单腿裸敞口是本策略的已知风险点（无强制 rebalance），单独告警
            if exposure.legs == 1 {
                tracing::warn!(
                    target: "metrics",
                    %symbol,
                    net_size = format!("{:.6}", exposure.net_size),
                    net_notional = format!("{:.2}", exposure.net_notional),
                    "metrics.single_leg_exposure：单腿裸敞口，等待后续信号中和"
                );
            }
        }

        // ---------- 订单 + 历史 ----------
        let total_session_pnl = self.total.total_pnl(session_position_value);
        prom.gauge("hft_total_equity", &[], total_equity);
        prom.gauge("hft_total_account_notional", &[], total_notional);
        prom.gauge("hft_position_gross_notional_total", &[], gross_notional);
        prom.gauge("hft_position_net_notional_abs_total", &[], abs_net_notional);
        prom.gauge("hft_symbols_with_position", &[], symbols_with_position as f64);
        prom.gauge("hft_single_leg_symbols", &[], single_leg_symbols as f64);
        prom.gauge("hft_unpriced_legs", &[], unpriced_legs as f64);
        prom.gauge("hft_pending_orders_total", &[], pending_orders as f64);
        prom.gauge("hft_rejected_orders_total", &[], self.total.rejected_orders as f64);
        prom.gauge("hft_fills_total", &[], self.total.fills as f64);
        prom.gauge("hft_forced_fills_total", &[], self.total.forced_fills as f64);
        prom.gauge("hft_volume_total", &[], self.total.volume);
        prom.gauge("hft_fee_total", &[], self.total.fee);
        prom.gauge("hft_session_pnl", &[], total_session_pnl);
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

        // ---------- 模拟账户 ----------
        for (account, stats) in &self.paper_stats {
            let account_label = account.to_string();
            prom.gauge("hft_paper_fills", &[("account", &account_label)], stats.fills as f64);
            prom.gauge("hft_paper_volume", &[("account", &account_label)], stats.volume);
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

        // 配置了 pushgateway 就把同一份数字推出去（异步、失败只记 warn，不拖垮引擎）
        if let Some(config) = &self.push {
            prometheus::spawn_push(config.clone(), prom.into_body());
        }

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
                price = fill.price,
                size = fill.size,
                fee = fill.fee,
                reason = ?fill.reason,
                ts = fill.timestamp,
                "metrics.recent_fill"
            );
        }
    }
}

impl Actor for MetricsActor {
    type Args = MetricsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(interval_ms = args.interval_ms, "MetricsActor started");

        Ok(Self {
            state: StateManager::new(&[], NO_ORDER_TIMEOUT),
            tracked: HashSet::new(),
            baseline: HashMap::new(),
            per_symbol: HashMap::new(),
            total: TradingStats::default(),
            untracked_events: 0,
            push: args.push,
            paper_stats: HashMap::new(),
            paper_accounts: HashMap::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // 停机前留一份最终快照，便于事后复盘
        self.report();
        tracing::info!("MetricsActor stopped");
        Ok(())
    }
}

// === Messages ===

/// 投产注册握手：要跟踪的 symbol 集合 + 它们的持仓基线（同一份快照，与 executor /
/// 对账镜像口径一致）。注册与基线**原子**到达 —— 不存在"已注册、基线未到"的窗口，
/// 也就不需要缓冲重放机制。
pub struct RegisterSymbols {
    pub symbols: Vec<Symbol>,
    pub baselines: Vec<crate::messaging::PositionBaseline>,
}

impl Message<RegisterSymbols> for MetricsActor {
    type Reply = ();

    async fn handle(&mut self, msg: RegisterSymbols, _ctx: &mut Context<Self, Self::Reply>) {
        tracing::info!(count = msg.symbols.len(), "MetricsActor tracking symbols");
        self.state.register_symbols(&msg.symbols);
        self.tracked.extend(msg.symbols);
        // 会话起始仓位基线（盈亏口径）与持仓镜像各记一份；重复注册（再晋升）由
        // record_baseline 的 or_insert 与 seed 的幂等各自兜住，首次值不被覆写
        for b in &msg.baselines {
            self.record_baseline(&b.position.symbol, b.position.exchange, b.position.size);
        }
        self.state.seed_positions(&msg.baselines);
    }
}

/// 行情事件入口（持仓估值 / 市场状态视图）
impl Message<MarketEvent> for MetricsActor {
    type Reply = ();

    async fn handle(&mut self, msg: MarketEvent, _ctx: &mut Context<Self, Self::Reply>) {
        // 跟踪范围外的 symbol 事件直接计数跳过：喂给 StateManager 会被当成"路由 bug"打 error，
        // 但对观测层这只是"不关心的 symbol"（分桶部署下必然出现），不是错误。
        if let Some(symbol) = msg.data.symbol() {
            if !self.tracked.contains(symbol) {
                self.untracked_events += 1;
                return;
            }
        }
        self.state.apply(&IncomeEvent::Market(msg));
    }
}

/// 账户事件入口：实盘进实盘视图（state/total），模拟账户按账户分账 —— 分道由事件
/// 自带的账户标签决定，不再依赖"来源总线"区分
impl Message<AccountEvent> for MetricsActor {
    type Reply = ();

    async fn handle(&mut self, msg: AccountEvent, _ctx: &mut Context<Self, Self::Reply>) {
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
            AccountData::Fill(fill) => {
                self.total.apply_fill(fill);
                self.per_symbol
                    .entry(fill.symbol.clone())
                    .or_insert_with(|| {
                        TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL)
                    })
                    .apply_fill(fill);
            }
            AccountData::OrderUpdate(update) => {
                self.total.apply_order_update(update);
                self.per_symbol
                    .entry(update.symbol.clone())
                    .or_insert_with(|| {
                        TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL)
                    })
                    .apply_order_update(update);
            }
            _ => {}
        }
        // 账户 / 持仓 / 挂单视图
        self.state.apply(&IncomeEvent::Account(msg));
    }
}

/// 定时器：输出报告
impl Message<StreamMessage<Instant, (), ()>> for MetricsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => self.report(),
            StreamMessage::Started(_) => tracing::debug!("Metrics report timer started"),
            // 观测组件故障**不得**拖垮交易：MetricsActor 是 spawn_link 到 manager 的，
            // 这里 kill 自己会经 on_link_died 让整个进程退出。定时器结束只记 error 继续存活，
            // 事件视图仍在更新，停机时的 on_stop 报告也还在。
            StreamMessage::Finished(_) => {
                tracing::error!("Metrics report timer finished, 后续周期报告停止（不影响交易）")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fill, FillReason, Position, Side};

    /// 投产握手的基线载荷（snapshot_req_ts=0；需要累加的 Fill 应带 local_ts > 0）
    fn baseline_of(symbol: &str, size: f64) -> crate::messaging::PositionBaseline {
        crate::messaging::PositionBaseline {
            position: Position {
                exchange: Exchange::Binance,
                symbol: symbol.to_string(),
                size,
            },
            snapshot_req_ts: 0,
        }
    }

    fn fill_data(exchange: Exchange, symbol: &str, side: Side, size: f64) -> AccountData {
        AccountData::Fill(Fill {
            exchange,
            symbol: symbol.to_string(),
            side,
            price: 100.0,
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: 0,
            fee: 0.0,
            reason: FillReason::Normal,
        })
    }

    /// 直接构造（绕过 actor 生命周期），用于测试纯状态累积逻辑
    fn actor() -> MetricsActor {
        MetricsActor {
            state: StateManager::new(&[], NO_ORDER_TIMEOUT),
            tracked: HashSet::new(),
            baseline: HashMap::new(),
            per_symbol: HashMap::new(),
            total: TradingStats::default(),
            untracked_events: 0,
            push: None,
            paper_stats: HashMap::new(),
            paper_accounts: HashMap::new(),
        }
    }

    /// 模拟账户事件按账户分账，**绝不**混进实盘视图（total/state）
    #[tokio::test]
    async fn paper_events_never_leak_into_live_view() {
        let mut metrics = actor();
        metrics.tracked.insert("BTC".to_string());
        metrics.state.register_symbols(&["BTC".to_string()]);

        let account = crate::domain::AccountId::Paper("BTC".to_string());
        let data = fill_data(Exchange::Binance, "BTC", Side::Long, 1.0);
        // 调 handler 委托的真实分账方法：将来有人在里面误加 state.apply，本测试会红
        metrics.apply_paper_event(&account, &data);

        assert_eq!(metrics.paper_stats[&account].fills, 1);
        assert_eq!(metrics.total.fills, 0, "模拟成交不得计入实盘累计");
        assert_eq!(
            metrics
                .state
                .symbol_state(&"BTC".to_string())
                .unwrap()
                .position_size(Exchange::Binance),
            0.0,
            "模拟成交不得进实盘持仓镜像"
        );
    }

    #[tokio::test]
    async fn baseline_records_only_the_first_position_snapshot() {
        let mut metrics = actor();
        metrics.tracked.insert("BTC".to_string());
        metrics.state.register_symbols(&["BTC".to_string()]);

        // 启动快照 2.0，随后又来一条 Position（不应覆盖基线）
        metrics.record_baseline(&"BTC".to_string(), Exchange::Binance, 2.0);
        metrics.record_baseline(&"BTC".to_string(), Exchange::Binance, 9.0);

        assert_eq!(
            metrics.baseline["BTC"][&Exchange::Binance],
            2.0,
            "基线只取第一次快照"
        );
    }

    #[tokio::test]
    async fn untracked_symbol_events_are_counted_not_applied() {
        let mut metrics = actor();
        metrics.tracked.insert("BTC".to_string());
        metrics.state.register_symbols(&["BTC".to_string()]);

        let ctx_free_data = fill_data(Exchange::Binance, "ETH", Side::Long, 1.0);
        // 手工走一遍 handler 的判定分支（不经 actor 运行时）
        if let Some(symbol) = ctx_free_data.symbol() {
            if !metrics.tracked.contains(symbol) {
                metrics.untracked_events += 1;
            }
        }

        assert_eq!(metrics.untracked_events, 1);
        assert_eq!(metrics.total.fills, 0);
    }

    #[tokio::test]
    async fn session_pnl_excludes_pre_existing_inventory() {
        let mut metrics = actor();
        let symbol = "BTC".to_string();
        metrics.tracked.insert(symbol.clone());
        metrics.state.register_symbols(&[symbol.clone()]);

        // 启动时已有 2 个多头（重启场景）：投产握手 seed 基线
        metrics.record_baseline(&symbol, Exchange::Binance, 2.0);
        metrics.state.seed_positions(&[baseline_of(&symbol, 2.0)]);
        // 喂 BBO 让持仓可估值
        metrics.state.apply(&IncomeEvent::market(
            0,
            0,
            crate::messaging::MarketData::BBO(crate::domain::BBO {
                exchange: Exchange::Binance,
                symbol: symbol.clone(),
                bid_price: 100.0,
                bid_qty: 1.0,
                ask_price: 100.0,
                ask_qty: 1.0,
                timestamp: 0,
            }),
        ));

        let state = metrics.state.symbol_state(&symbol).unwrap();
        let exposure = exposure_of(state, metrics.baseline.get(&symbol));

        // 全额市值 200，但本次会话未成交 → session PnL 为 0，而非凭空 +200
        assert!((exposure.net_notional - 200.0).abs() < 1e-9);
        assert!(exposure.session_notional_delta.abs() < 1e-9);
        assert!(metrics.total.total_pnl(exposure.session_notional_delta).abs() < 1e-9);
    }

    #[tokio::test]
    async fn session_pnl_tracks_this_session_trades() {
        let mut metrics = actor();
        let symbol = "BTC".to_string();
        metrics.tracked.insert(symbol.clone());
        metrics.state.register_symbols(&[symbol.clone()]);

        metrics.record_baseline(&symbol, Exchange::Binance, 2.0);
        metrics.state.seed_positions(&[baseline_of(&symbol, 2.0)]);

        // 本次会话又买 1 个（价 100），随后 BBO 报 110；
        // local_ts 晚于快照请求时刻，不会被 seed 的防双计过滤
        let data = fill_data(Exchange::Binance, &symbol, Side::Long, 1.0);
        if let AccountData::Fill(f) = &data {
            metrics.total.apply_fill(f);
        }
        metrics.state.apply(&IncomeEvent::account(
            crate::domain::AccountId::Live,
            1,
            1,
            data,
        ));
        metrics.state.apply(&IncomeEvent::market(
            0,
            0,
            crate::messaging::MarketData::BBO(crate::domain::BBO {
                exchange: Exchange::Binance,
                symbol: symbol.clone(),
                bid_price: 110.0,
                bid_qty: 1.0,
                ask_price: 110.0,
                ask_qty: 1.0,
                timestamp: 0,
            }),
        ));

        let state = metrics.state.symbol_state(&symbol).unwrap();
        let exposure = exposure_of(state, metrics.baseline.get(&symbol));

        // 会话内新增 1 个、现价 110 → 存货变化 +110，现金流 -100 → 浮盈 10
        assert!((exposure.session_notional_delta - 110.0).abs() < 1e-9);
        assert!((metrics.total.total_pnl(exposure.session_notional_delta) - 10.0).abs() < 1e-9);
    }

    // ===== 跨所估值（V7 之前住在 SymbolState 上，随口径一起搬来） =====

    const EXPOSURE_SYMBOL: &str = "BTC";

    fn exposure_bbo(exchange: Exchange, mid: f64) -> crate::domain::BBO {
        crate::domain::BBO {
            exchange,
            symbol: EXPOSURE_SYMBOL.to_string(),
            bid_price: mid,
            bid_qty: 1.0,
            ask_price: mid,
            ask_qty: 1.0,
            timestamp: 0,
        }
    }

    /// 经**生产路径**构造夹具：仓位走投产 seed、盘口走行情事件。
    /// 直接写字段更省事，但那样测的就不是线上跑的那条路。
    fn state_with(positions: &[(Exchange, f64)], priced: &[(Exchange, f64)]) -> SymbolState {
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
            state.apply(&IncomeEvent::market(
                0,
                0,
                crate::messaging::MarketData::BBO(exposure_bbo(exchange, mid)),
            ));
        }
        state
    }

    #[test]
    fn hedged_position_has_near_zero_net_notional() {
        let state = state_with(
            &[(Exchange::Binance, 1.0), (Exchange::OKX, -1.0)],
            &[(Exchange::Binance, 100.0), (Exchange::OKX, 100.0)],
        );

        let exposure = exposure_of(&state, None);

        assert_eq!(exposure.legs, 2);
        assert!(exposure.net_notional.abs() < 1e-9);
        assert!((exposure.gross_notional - 200.0).abs() < 1e-9);
        assert_eq!(exposure.unpriced_legs, 0);
    }

    #[test]
    fn baseline_position_is_excluded_from_session_delta() {
        let state = state_with(&[(Exchange::Binance, 3.0)], &[(Exchange::Binance, 100.0)]);
        let baseline = HashMap::from([(Exchange::Binance, 2.0)]);

        let exposure = exposure_of(&state, Some(&baseline));

        // 全额估值 300，但本次会话只新增了 1 个 → 100
        assert!((exposure.net_notional - 300.0).abs() < 1e-9);
        assert!((exposure.session_notional_delta - 100.0).abs() < 1e-9);
    }

    #[test]
    fn missing_baseline_defaults_to_zero() {
        let state = state_with(&[(Exchange::Binance, 3.0)], &[(Exchange::Binance, 100.0)]);
        let exposure = exposure_of(&state, Some(&HashMap::new()));
        assert!((exposure.session_notional_delta - 300.0).abs() < 1e-9);
    }

    /// 缺 BBO 的腿显式暴露成 `unpriced_legs`，不用兜底价格伪造一个看似完整的估值
    #[test]
    fn missing_bbo_is_surfaced_not_silently_valued_at_zero() {
        let state = state_with(&[(Exchange::Binance, 1.0)], &[]);

        let exposure = exposure_of(&state, None);

        assert_eq!(exposure.legs, 1);
        assert_eq!(exposure.unpriced_legs, 1);
        assert_eq!(exposure.gross_notional, 0.0);
    }

    #[test]
    fn zero_positions_are_ignored() {
        let state = state_with(&[(Exchange::Binance, 0.0)], &[(Exchange::Binance, 100.0)]);
        assert_eq!(exposure_of(&state, None), SymbolExposure::default());
    }
}

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
//! - 跨所持仓估值复用 [`SymbolState::exposure`]，累计统计复用 [`TradingStats`]
//! - 盈亏为**本次会话**口径：现金流 + 相对会话起始仓位的存货估值变化，
//!   这样 docker 重启后不会把重启前就持有的存货算成假盈利
//! - 只输出日志，不引入外部依赖（Prometheus / Slack 等上报另行决策，见 docs/todo.md）

use crate::domain::{Exchange, Symbol, TradingStats};
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
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
    /// 取自启动期的 [`ExchangeEventData::PositionBaseline`]（每个 (symbol, 所) 只记第一条，
    /// 与 `SymbolState` 的基线语义一致）。用于把重启前的存货从盈亏里剔除。
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
}

impl MetricsActor {
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
        for (exchange, info) in self.state.account_infos() {
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
            let exposure = symbol_state.exposure(self.baseline.get(symbol));
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

/// 注册需要跟踪的 symbol 集合
pub struct RegisterSymbols(pub Vec<Symbol>);

impl Message<RegisterSymbols> for MetricsActor {
    type Reply = ();

    async fn handle(&mut self, msg: RegisterSymbols, _ctx: &mut Context<Self, Self::Reply>) {
        tracing::info!(count = msg.0.len(), "MetricsActor tracking symbols");
        self.state.register_symbols(&msg.0);
        self.tracked.extend(msg.0);
    }
}

/// 全量事件流入口
impl Message<IncomeEvent> for MetricsActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        // 跟踪范围外的 symbol 事件直接计数跳过：喂给 StateManager 会被当成"路由 bug"打 error，
        // 但对观测层这只是"不关心的 symbol"（分桶部署下必然出现），不是错误。
        if let Some(symbol) = msg.symbol() {
            if !self.tracked.contains(symbol) {
                self.untracked_events += 1;
                return;
            }
        }

        // 会话起始仓位基线：必须在 state.apply 之前取，那之后就分不清"快照初始值"和"成交累加值"了
        if let ExchangeEventData::PositionBaseline(position) = &msg.data {
            self.record_baseline(&position.symbol, position.exchange, position.size);
        }

        // 账户 / 持仓 / 挂单视图
        self.state.apply(&msg);

        // 累计统计
        match &msg.data {
            ExchangeEventData::Fill(fill) => {
                self.total.apply_fill(fill);
                self.per_symbol
                    .entry(fill.symbol.clone())
                    .or_insert_with(|| {
                        TradingStats::with_recent_capacity(NO_PER_SYMBOL_FILL_DETAIL)
                    })
                    .apply_fill(fill);
            }
            ExchangeEventData::OrderUpdate(update) => {
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

    fn position_event(exchange: Exchange, symbol: &str, size: f64) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: ExchangeEventData::PositionBaseline(Position {
                exchange,
                symbol: symbol.to_string(),
                size,
                entry_price: 100.0,
                unrealized_pnl: 0.0,
            }),
        }
    }

    fn fill_event(exchange: Exchange, symbol: &str, side: Side, size: f64) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: ExchangeEventData::Fill(Fill {
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
            }),
        }
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
        }
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

        let ctx_free_event = fill_event(Exchange::Binance, "ETH", Side::Long, 1.0);
        // 手工走一遍 handler 的判定分支（不经 actor 运行时）
        if let Some(symbol) = ctx_free_event.symbol() {
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

        // 启动时已有 2 个多头（重启场景）
        let snapshot = position_event(Exchange::Binance, &symbol, 2.0);
        metrics.record_baseline(&symbol, Exchange::Binance, 2.0);
        metrics.state.apply(&snapshot);
        // 喂 BBO 让持仓可估值
        metrics.state.apply(&IncomeEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: ExchangeEventData::BBO(crate::domain::BBO {
                exchange: Exchange::Binance,
                symbol: symbol.clone(),
                bid_price: 100.0,
                bid_qty: 1.0,
                ask_price: 100.0,
                ask_qty: 1.0,
                timestamp: 0,
            }),
        });

        let state = metrics.state.symbol_state(&symbol).unwrap();
        let exposure = state.exposure(metrics.baseline.get(&symbol));

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
        metrics.state.apply(&position_event(Exchange::Binance, &symbol, 2.0));

        // 本次会话又买 1 个（价 100），随后 BBO 报 110
        let fill = fill_event(Exchange::Binance, &symbol, Side::Long, 1.0);
        if let ExchangeEventData::Fill(f) = &fill.data {
            metrics.total.apply_fill(f);
        }
        metrics.state.apply(&fill);
        metrics.state.apply(&IncomeEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: ExchangeEventData::BBO(crate::domain::BBO {
                exchange: Exchange::Binance,
                symbol: symbol.clone(),
                bid_price: 110.0,
                bid_qty: 1.0,
                ask_price: 110.0,
                ask_qty: 1.0,
                timestamp: 0,
            }),
        });

        let state = metrics.state.symbol_state(&symbol).unwrap();
        let exposure = state.exposure(metrics.baseline.get(&symbol));

        // 会话内新增 1 个、现价 110 → 存货变化 +110，现金流 -100 → 浮盈 10
        assert!((exposure.session_notional_delta - 110.0).abs() < 1e-9);
        assert!((metrics.total.total_pnl(exposure.session_notional_delta) - 10.0).abs() < 1e-9);
    }
}

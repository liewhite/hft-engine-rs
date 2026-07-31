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
//! - 累计统计复用 [`TradingStats`]（domain 层纯数据），可独立单测
//! - 只输出日志，不引入外部依赖（Prometheus / Slack 等上报另行决策，见 docs/todo.md）

use crate::domain::{Symbol, TradingStats};
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager, SymbolState};
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

/// 仓位视为非零的阈值
const POSITION_EPSILON: f64 = 1e-10;

/// MetricsActor 初始化参数
pub struct MetricsActorArgs {
    /// 报告间隔 (毫秒)
    pub interval_ms: u64,
}

/// 单 symbol 的持仓汇总（纯数据）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SymbolExposure {
    /// 各交易所带符号仓位之和（净敞口，币本位）
    pub net_size: f64,
    /// Σ |仓位| × mid（总名义敞口）
    pub gross_notional: f64,
    /// Σ 仓位 × mid（净名义敞口，理想对冲下应接近 0）
    pub net_notional: f64,
    /// 有非零仓位的交易所个数
    pub legs: usize,
    /// 有仓位但缺少 BBO、无法估值的交易所个数（估值不完整的提示）
    pub unpriced_legs: usize,
}

/// 汇总单 symbol 在各交易所的持仓与估值
///
/// 纯函数：仅依赖入参。缺 BBO 的腿不参与估值，但计入 `unpriced_legs`——
/// 宁可显式暴露"估值不完整"，也不用兜底价格伪造一个看似完整的数字。
pub fn symbol_exposure(state: &SymbolState) -> SymbolExposure {
    let mut exposure = SymbolExposure::default();
    for (exchange, position) in &state.positions {
        if position.size.abs() < POSITION_EPSILON {
            continue;
        }
        exposure.legs += 1;
        exposure.net_size += position.size;
        match state.bbo(*exchange) {
            Some(bbo) => {
                let mid = bbo.mid_price();
                exposure.gross_notional += position.size.abs() * mid;
                exposure.net_notional += position.size * mid;
            }
            None => exposure.unpriced_legs += 1,
        }
    }
    exposure
}

/// MetricsActor - 交易指标观测
pub struct MetricsActor {
    /// 账户 / 持仓 / 挂单视图（复用策略侧同一实现）
    state: StateManager,
    /// 正在跟踪的 symbol（由 [`RegisterSymbols`] 注册）
    tracked: HashSet<Symbol>,
    /// per-symbol 累计成交统计
    per_symbol: HashMap<Symbol, TradingStats>,
    /// 全账户累计成交统计
    total: TradingStats,
    /// 落在跟踪范围外、被跳过的 symbol 事件数
    ///
    /// 不静默丢弃：计数并在报告里暴露。持续增长说明注册的 symbol 集合与实际事件流不一致
    /// （例如上层忘了发 [`RegisterSymbols`]）。
    untracked_events: u64,
}

impl MetricsActor {
    /// 输出一次完整报告
    fn report(&self) {
        // ---------- 账户 ----------
        let mut total_equity = 0.0;
        let mut total_notional = 0.0;
        for (exchange, info) in self.state.account_infos() {
            let leverage = if info.equity > 0.0 {
                info.notional / info.equity
            } else {
                0.0
            };
            total_equity += info.equity;
            total_notional += info.notional;
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
        let mut position_value = 0.0;
        let mut gross_notional = 0.0;
        let mut abs_net_notional = 0.0;
        let mut symbols_with_position = 0usize;
        let mut single_leg_symbols = 0usize;
        let mut unpriced_legs = 0usize;
        let mut pending_orders = 0usize;

        for (symbol, symbol_state) in self.state.symbol_states() {
            let exposure = symbol_exposure(symbol_state);
            let pending = symbol_state.pending_orders().count();
            pending_orders += pending;
            position_value += exposure.net_notional;
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
                pnl = format!(
                    "{:.4}",
                    stats
                        .map(|s| s.total_pnl(exposure.net_notional))
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
        tracing::info!(
            target: "metrics",
            exchanges = self.state.account_infos().count(),
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
            total_pnl = format!("{:.4}", self.total.total_pnl(position_value)),
            "metrics.summary"
        );

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
            per_symbol: HashMap::new(),
            total: TradingStats::default(),
            untracked_events: 0,
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

/// 注册需要跟踪的 symbol 集合（由上层在创建策略时告知）
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
        // 但对观测层这只是"不关心的 symbol"，不是错误。
        if let Some(symbol) = msg.symbol() {
            if !self.tracked.contains(symbol) {
                self.untracked_events += 1;
                return;
            }
        }

        // 账户 / 持仓 / 挂单视图
        self.state.apply(&msg);

        // 累计统计
        match &msg.data {
            ExchangeEventData::Fill(fill) => {
                self.total.apply_fill(fill);
                self.per_symbol
                    .entry(fill.symbol.clone())
                    .or_default()
                    .apply_fill(fill);
            }
            ExchangeEventData::OrderUpdate(update) => {
                self.total.apply_order_update(update);
                self.per_symbol
                    .entry(update.symbol.clone())
                    .or_default()
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
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => self.report(),
            StreamMessage::Started(_) => tracing::debug!("Metrics report timer started"),
            StreamMessage::Finished(_) => {
                tracing::error!("Metrics report timer unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, Position, BBO};

    fn bbo(exchange: Exchange, bid: f64, ask: f64) -> BBO {
        BBO {
            exchange,
            symbol: "BTC".to_string(),
            bid_price: bid,
            bid_qty: 1.0,
            ask_price: ask,
            ask_qty: 1.0,
            timestamp: 0,
        }
    }

    fn position(exchange: Exchange, size: f64) -> Position {
        Position {
            exchange,
            symbol: "BTC".to_string(),
            size,
            entry_price: 100.0,
            unrealized_pnl: 0.0,
        }
    }

    fn state_with(positions: Vec<Position>, bbos: Vec<BBO>) -> SymbolState {
        let mut state = SymbolState::new("BTC".to_string());
        for p in positions {
            state.positions.insert(p.exchange, p);
        }
        for b in bbos {
            state.bbos.insert(b.exchange, b);
        }
        state
    }

    #[test]
    fn hedged_position_has_near_zero_net_notional() {
        let state = state_with(
            vec![
                position(Exchange::Binance, 1.0),
                position(Exchange::OKX, -1.0),
            ],
            vec![
                bbo(Exchange::Binance, 100.0, 100.0),
                bbo(Exchange::OKX, 100.0, 100.0),
            ],
        );

        let exposure = symbol_exposure(&state);
        assert_eq!(exposure.legs, 2);
        assert!(exposure.net_notional.abs() < 1e-9);
        assert!((exposure.gross_notional - 200.0).abs() < 1e-9);
        assert_eq!(exposure.unpriced_legs, 0);
    }

    #[test]
    fn single_leg_is_reported_as_one_leg_exposure() {
        let state = state_with(
            vec![position(Exchange::Binance, 2.0)],
            vec![bbo(Exchange::Binance, 100.0, 102.0)],
        );

        let exposure = symbol_exposure(&state);
        assert_eq!(exposure.legs, 1);
        assert!((exposure.net_size - 2.0).abs() < 1e-9);
        assert!((exposure.net_notional - 202.0).abs() < 1e-9);
    }

    #[test]
    fn missing_bbo_is_surfaced_not_silently_valued_at_zero() {
        let state = state_with(vec![position(Exchange::Binance, 1.0)], vec![]);

        let exposure = symbol_exposure(&state);
        assert_eq!(exposure.legs, 1);
        assert_eq!(exposure.unpriced_legs, 1);
        assert_eq!(exposure.gross_notional, 0.0);
    }

    #[test]
    fn zero_positions_are_ignored() {
        let state = state_with(
            vec![position(Exchange::Binance, 0.0)],
            vec![bbo(Exchange::Binance, 100.0, 100.0)],
        );

        assert_eq!(symbol_exposure(&state), SymbolExposure::default());
    }
}

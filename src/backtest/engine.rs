//! 虚拟时间回测引擎 —— 单线程、确定性。
//!
//! 与实盘共享**全部领域逻辑**：撮合用纯状态机 [`SimState`]、策略步用 [`StrategyRunner`]、
//! 账本用 [`crate::sim::Ledger`]。差异只在驱动层——实盘是并发墙钟，回测是单线程虚拟时间
//! 优先队列：把"延迟 D 后做某事"实现为"入队到 now+D"。同一输入必得同一结果。
//!
//! 时间推进：行情事件来自 [`MarketDataSource`] (已全局时间有序)，按其 exchange_ts 注入；
//! 注入/撮合产生的回流按 `exchange_to_strategy_delay_ms` 入队投递给策略，策略下单按
//! `order_to_exchange_delay_ms` 入队到达撮合。主循环始终处理"源 peek 与队列 peek 中较早者"，
//! 同刻先排空队列 (先消化既有效果再注入新行情)。
//!
//! 账户净值：周期性 (clock_interval_ms) 由当前账本计算 AccountInfo 投递给策略 (等价实盘的
//! accountRefresh)，并在首个事件时先投递一次初始净值，否则依赖净值的策略不会动作。

use crate::backtest::source::MarketDataSource;
use crate::domain::{Exchange, Order, OrderId, Position, Symbol, Timestamp};
use crate::engine::StrategyRunner;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::sim::{SimConfig, SimState};
use crate::strategy::OutcomeEvent;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

/// 回测结果汇总。`realized_pnl` 为已实现盈亏 (= 账本现金增量)，`final_equity` 含未实现。
#[derive(Debug, Clone, PartialEq)]
pub struct BacktestResult {
    pub initial_balance: f64,
    pub final_equity: f64,
    pub realized_pnl: f64,
    pub fills: u64,
    pub market_events: u64,
    /// 非空持仓 (按 symbol 升序，保证确定性比对)
    pub positions: Vec<Position>,
    pub first_ts: Timestamp,
    pub last_ts: Timestamp,
}

/// 旁路观察者：与策略共享同一事件流但不参与撮合 (如成交记录器)。
type Observer<'a> = Box<dyn FnMut(&IncomeEvent) + 'a>;

/// 队列内的延迟动作。
enum Action {
    /// 交易所侧事件到达策略/观察者
    Deliver(IncomeEvent),
    OrderArrive(Order, OrderId),
    CancelArrive(OrderId),
    Clock,
}

/// 最小堆元素：先按时间、同刻按 seq (入队序) -> 完全确定。
struct Scheduled {
    time: Timestamp,
    seq: u64,
    action: Action,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.seq == other.seq
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        // 仅按 (time, seq) 排序；seq 全局唯一故为全序
        self.time.cmp(&other.time).then(self.seq.cmp(&other.seq))
    }
}

/// 单线程虚拟时间回测引擎。
pub struct BacktestEngine<'a> {
    exchange: Exchange,
    source: &'a dyn MarketDataSource,
    runners: Vec<StrategyRunner>,
    config: SimConfig,
    observers: Vec<Observer<'a>>,
    clock_interval_ms: u64,

    // ---- 运行期状态 ----
    state: SimState,
    pq: BinaryHeap<Reverse<Scheduled>>,
    now: Timestamp,
    seq_gen: u64,
    order_id_gen: u64,
    fill_count: u64,
    market_events: u64,
    first_ts: Timestamp,
    /// 源是否仍有行情待注入 (决定时钟是否续期, 保证终止)
    more_data: bool,
}

impl<'a> BacktestEngine<'a> {
    pub fn new(
        exchange: Exchange,
        source: &'a dyn MarketDataSource,
        runners: Vec<StrategyRunner>,
        config: SimConfig,
    ) -> Self {
        let state = SimState::empty(
            config.initial_balance_usdt,
            config.maker_fee_rate,
            config.taker_fee_rate,
        );
        Self {
            exchange,
            source,
            runners,
            config,
            observers: Vec::new(),
            clock_interval_ms: 1000,
            state,
            pq: BinaryHeap::new(),
            now: 0,
            seq_gen: 0,
            order_id_gen: 0,
            fill_count: 0,
            market_events: 0,
            first_ts: 0,
            more_data: true,
        }
    }

    /// 注册旁路观察者 (如成交记录器)，与策略共享同一事件流但不参与撮合。
    pub fn with_observer(mut self, obs: impl FnMut(&IncomeEvent) + 'a) -> Self {
        self.observers.push(Box::new(obs));
        self
    }

    /// 设置账户净值刷新周期 (默认 1000ms)。
    pub fn with_clock_interval(mut self, ms: u64) -> Self {
        self.clock_interval_ms = ms;
        self
    }

    fn schedule(&mut self, time: Timestamp, action: Action) {
        self.seq_gen += 1;
        self.pq.push(Reverse(Scheduled {
            time,
            seq: self.seq_gen,
            action,
        }));
    }

    /// 跑完整个数据源，返回汇总结果。
    pub fn run(&mut self) -> BacktestResult {
        let mut src = self.source.events().peekable();
        let first = match src.peek() {
            Some(ev) => ev.exchange_ts,
            None => {
                tracing::warn!("no market data; empty backtest");
                let bal = self.config.initial_balance_usdt;
                return BacktestResult {
                    initial_balance: bal,
                    final_equity: bal,
                    realized_pnl: 0.0,
                    fills: 0,
                    market_events: 0,
                    positions: Vec::new(),
                    first_ts: 0,
                    last_ts: 0,
                };
            }
        };
        self.first_ts = first;
        self.now = first;
        let init_account = self.account_info_event(self.now);
        self.deliver(init_account); // 初始净值, 让依赖 equity 的策略可启动
        self.schedule(self.now + self.clock_interval_ms, Action::Clock);

        while src.peek().is_some() || !self.pq.is_empty() {
            self.more_data = src.peek().is_some();
            let src_ts = src.peek().map(|e| e.exchange_ts);
            let pq_ts = self.pq.peek().map(|r| r.0.time);
            match (src_ts, pq_ts) {
                (Some(st), Some(pt)) => {
                    if pt <= st {
                        self.run_queued();
                    } else {
                        let ev = src.next().unwrap();
                        self.ingest(ev);
                    }
                }
                (Some(_), None) => {
                    let ev = src.next().unwrap();
                    self.ingest(ev);
                }
                (None, Some(_)) => self.run_queued(),
                (None, None) => break,
            }
        }

        let mut positions = {
            let state = &self.state;
            state.ledger.open_positions(|s| state.mark_of(s))
        };
        positions.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        let final_equity = {
            let state = &self.state;
            state.ledger.equity(|s| state.mark_of(s))
        };
        let result = BacktestResult {
            initial_balance: self.config.initial_balance_usdt,
            final_equity,
            realized_pnl: self.state.ledger.cash - self.config.initial_balance_usdt,
            fills: self.fill_count,
            market_events: self.market_events,
            positions,
            first_ts: self.first_ts,
            last_ts: self.now,
        };
        tracing::info!(
            events = result.market_events,
            fills = result.fills,
            realized_pnl = result.realized_pnl,
            final_equity = result.final_equity,
            "backtest done"
        );
        result
    }

    /// 注入一条历史行情：推进时间、撮合、回流按延迟入队投递。
    fn ingest(&mut self, ev: IncomeEvent) {
        self.now = ev.exchange_ts;
        self.market_events += 1;
        let replies = self.state.on_market(self.exchange, &ev);
        self.enqueue_replies(replies);
    }

    /// 把撮合回流事件按 ex->strat 延迟入队投递给策略。
    fn enqueue_replies(&mut self, replies: Vec<IncomeEvent>) {
        let delay = self.config.exchange_to_strategy_delay_ms;
        for r in replies {
            self.schedule(self.now + delay, Action::Deliver(r));
        }
    }

    fn run_queued(&mut self) {
        let Reverse(s) = self.pq.pop().expect("run_queued on empty queue");
        self.now = s.time;
        match s.action {
            Action::Deliver(ev) => self.deliver(ev),
            Action::OrderArrive(order, id) => {
                let replies = self.state.on_order_arrived(self.now, self.exchange, &order, &id);
                self.enqueue_replies(replies);
            }
            Action::CancelArrive(id) => {
                let replies = self.state.on_cancel_arrived(self.now, self.exchange, &id);
                self.enqueue_replies(replies);
            }
            Action::Clock => {
                let clock = IncomeEvent {
                    exchange_ts: self.now,
                    local_ts: self.now,
                    data: ExchangeEventData::Clock,
                };
                self.deliver(clock);
                let account = self.account_info_event(self.now);
                self.deliver(account); // 周期刷新净值, 等价实盘 accountRefresh
                // 仅在仍有行情待注入时续期, 源耗尽则停摆让队列自然排空 -> 保证终止
                if self.more_data {
                    self.schedule(self.now + self.clock_interval_ms, Action::Clock);
                }
            }
        }
    }

    /// 把事件投递给观察者与各策略；策略产出的信号按下单延迟入队到达撮合。
    fn deliver(&mut self, ev: IncomeEvent) {
        if matches!(ev.data, ExchangeEventData::Fill(_)) {
            self.fill_count += 1;
        }
        for i in 0..self.observers.len() {
            (self.observers[i])(&ev);
        }
        let order_delay = self.config.order_to_exchange_delay_ms;
        for i in 0..self.runners.len() {
            if !self.runners[i].accepts(&ev) {
                continue;
            }
            let outcomes = self.runners[i].on_event(&ev);
            for outcome in outcomes {
                match outcome {
                    OutcomeEvent::PlaceOrders { orders, .. } => {
                        for o in orders {
                            self.order_id_gen += 1;
                            let id = self.order_id_gen.to_string();
                            self.schedule(self.now + order_delay, Action::OrderArrive(o, id));
                        }
                    }
                    OutcomeEvent::CancelOrder { order_id, .. } => {
                        self.schedule(self.now + order_delay, Action::CancelArrive(order_id));
                    }
                }
            }
        }
    }

    fn account_info_event(&self, ts: Timestamp) -> IncomeEvent {
        let state = &self.state;
        let equity = state.ledger.equity(|s: &Symbol| state.mark_of(s));
        let notional = state.ledger.notional(|s: &Symbol| state.mark_of(s));
        IncomeEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: ExchangeEventData::AccountInfo {
                exchange: self.exchange,
                equity,
                notional,
            },
        }
    }
}

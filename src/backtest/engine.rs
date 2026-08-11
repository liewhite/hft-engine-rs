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
use crate::domain::{Exchange, Order, OrderId, Price, Symbol, SymbolMeta, Timestamp};
use crate::engine::StrategyRunner;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::sim::{SimConfig, SimState};
use crate::strategy::OutcomeEvent;
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::sync::Arc;

/// 回测结束时的一笔持仓：数量取自账本，均价与浮盈是**本地记账**的结果
/// （域模型 [`Position`] 只有数量，见其文档）。
#[derive(Debug, Clone, PartialEq)]
pub struct BacktestPosition {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub size: crate::domain::Quantity,
    pub entry_price: Price,
    pub unrealized_pnl: f64,
}

/// 回测结果汇总。`realized_pnl` 为已实现盈亏 (= 账本现金增量)，`final_equity` 含未实现。
#[derive(Debug, Clone, PartialEq)]
pub struct BacktestResult {
    pub initial_balance: f64,
    pub final_equity: f64,
    pub realized_pnl: f64,
    pub fills: u64,
    pub market_events: u64,
    /// 非空持仓 (按 symbol 升序，保证确定性比对)
    pub positions: Vec<BacktestPosition>,
    pub first_ts: Timestamp,
    pub last_ts: Timestamp,
}

/// 旁路观察者：与策略共享同一事件流但不参与撮合 (如成交记录器)。
type Observer<'a> = Box<dyn FnMut(&IncomeEvent) + 'a>;

/// Outcome 旁路观察者：策略每产出一条信号立即回调 (runner 下标, 信号)。
/// 等价实盘 Outcome 总线的外部订阅者（`SubscribeOutcome`）—— 两个方向各有各的观察口，
/// 不把 outcome 包装成 income 混进 [`Observer`]（那会丢失方向与发出者归属）。
type OutcomeObserver<'a> = Box<dyn FnMut(usize, &OutcomeEvent) + 'a>;

/// 队列内的延迟动作。
enum Action {
    /// 交易所侧事件到达策略/观察者
    Deliver(IncomeEvent),
    OrderArrive(Order, OrderId),
    CancelArrive(Exchange, OrderId),
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
///
/// # 多交易所：按事件自身的 exchange 路由
///
/// 每个交易所一份 [`SimState`]（一份实例只代表一个所，见其类型文档），在该所首条行情
/// 到达时惰性创建、各以 `initial_balance_usdt` 起步（等价"每个所各入金一份"）。行情按
/// 事件自带的 exchange 送进对应柜台 —— 此前引擎持有单个 exchange 字段并忽略事件自身的
/// 归属，跨所策略（如 spread_arb）无法回测。`BTreeMap` 保证遍历顺序确定（净值事件的
/// 投递序影响确定性）。
pub struct BacktestEngine<'a> {
    source: &'a dyn MarketDataSource,
    runners: Vec<StrategyRunner>,
    config: SimConfig,
    /// 市场规则来源，透传给各所柜台（[`SimState`] 在订单到达时校验下界并取整 ——
    /// 保证实盘必拒的单回测也拒）。与 `StrategyRunner` 持有的是同一张表。
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    observers: Vec<Observer<'a>>,
    outcome_observers: Vec<OutcomeObserver<'a>>,
    clock_interval_ms: u64,

    // ---- 运行期状态 ----
    states: BTreeMap<Exchange, SimState>,
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
        source: &'a dyn MarketDataSource,
        runners: Vec<StrategyRunner>,
        config: SimConfig,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    ) -> Self {
        Self {
            source,
            runners,
            config,
            symbol_metas,
            observers: Vec::new(),
            outcome_observers: Vec::new(),
            clock_interval_ms: 1000,
            states: BTreeMap::new(),
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

    /// 注册 Outcome 旁路观察者：策略每产出一条信号（下单/撤单/Emit）立即回调，
    /// 带 runner 下标标识发出者。等价实盘经 `SubscribeOutcome` 订阅 Outcome 总线。
    pub fn with_outcome_observer(mut self, obs: impl FnMut(usize, &OutcomeEvent) + 'a) -> Self {
        self.outcome_observers.push(Box::new(obs));
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
        // 初始净值随各所柜台的惰性创建投递（见 ensure_state），让依赖 equity 的策略可启动
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

        // 回测结果里的持仓带上本地账本的均价与浮盈（BookSnapshot），并补上所属交易所 ——
        // 域模型 Position 只有数量（见其文档），盈亏是账本概念
        let mut positions: Vec<BacktestPosition> = self
            .states
            .iter()
            .flat_map(|(exchange, state)| {
                state
                    .ledger
                    .open_positions(|s| state.mark_of(s))
                    .into_iter()
                    .map(move |p| BacktestPosition {
                        exchange: *exchange,
                        symbol: p.symbol,
                        size: p.size,
                        entry_price: p.entry_price,
                        unrealized_pnl: p.unrealized_pnl,
                    })
            })
            .collect();
        positions.sort_by(|a, b| a.symbol.cmp(&b.symbol).then(a.exchange.cmp(&b.exchange)));
        let final_equity: f64 = self
            .states
            .values()
            .map(|state| state.ledger.equity(|s| state.mark_of(s)))
            .sum();
        // 每个所各入金一份，总初始资金随实际出现的所数走（无行情时保持单份，见上方早退分支）
        let initial_balance =
            self.config.initial_balance_usdt * (self.states.len().max(1) as f64);
        let total_cash: f64 = self.states.values().map(|s| s.ledger.cash).sum();
        let result = BacktestResult {
            initial_balance,
            final_equity,
            realized_pnl: total_cash - initial_balance,
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

    /// 注入一条历史行情：按事件自身的 exchange 路由到对应柜台，撮合、回流按延迟入队投递。
    fn ingest(&mut self, ev: IncomeEvent) {
        self.now = ev.exchange_ts;
        self.market_events += 1;
        let Some(exchange) = ev.exchange() else {
            // 无交易所归属的源事件不参与撮合，按入向延迟投递给策略
            // （与撮合回流同一口径，见 enqueue_replies）
            let delay = self.config.exchange_to_strategy_delay_ms;
            self.schedule(self.now + delay, Action::Deliver(ev));
            return;
        };
        self.ensure_state(exchange);
        let replies = self
            .states
            .get_mut(&exchange)
            .expect("ensure_state 刚创建")
            .on_market(&ev);
        self.enqueue_replies(replies);
    }

    /// 确保该所的柜台存在；首次创建时先投递一次该所的初始净值
    /// （在该所任何行情/回报之前送达，依赖 equity 的策略才能启动）。
    fn ensure_state(&mut self, exchange: Exchange) {
        if self.states.contains_key(&exchange) {
            return;
        }
        self.states.insert(
            exchange,
            SimState::empty(
                exchange,
                self.config.initial_balance_usdt,
                self.config.maker_fee_rate,
                self.config.taker_fee_rate,
                self.symbol_metas.clone(),
            ),
        );
        let info = self.account_info_event(exchange, self.now);
        self.schedule(self.now, Action::Deliver(info));
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
                // 按订单自身的 exchange 路由；该所若还没有柜台（无任何行情先行）也照建 ——
                // 市价单会因"无行情参考价"被柜台如实拒掉，而不是静默落进别的所
                let exchange = order.exchange;
                self.ensure_state(exchange);
                let replies = self
                    .states
                    .get_mut(&exchange)
                    .expect("ensure_state 刚创建")
                    .on_order_arrived(self.now, &order, &id);
                self.enqueue_replies(replies);
            }
            Action::CancelArrive(exchange, id) => {
                self.ensure_state(exchange);
                let replies = self
                    .states
                    .get_mut(&exchange)
                    .expect("ensure_state 刚创建")
                    .on_cancel_arrived(self.now, &id);
                self.enqueue_replies(replies);
            }
            Action::Clock => {
                let clock = IncomeEvent {
                    exchange_ts: self.now,
                    local_ts: self.now,
                    data: ExchangeEventData::Clock,
                };
                self.deliver(clock);
                // 周期刷新各所净值, 等价实盘 accountRefresh（BTreeMap 保证投递序确定）
                let exchanges: Vec<Exchange> = self.states.keys().copied().collect();
                for exchange in exchanges {
                    let account = self.account_info_event(exchange, self.now);
                    self.deliver(account);
                }
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
                // 全部信号先过 outcome 观察者（与实盘 Outcome 总线"订阅者看到一切"同构）
                for j in 0..self.outcome_observers.len() {
                    (self.outcome_observers[j])(i, &outcome);
                }
                match outcome {
                    OutcomeEvent::PlaceOrders { orders, .. } => {
                        for o in orders {
                            self.order_id_gen += 1;
                            let id = self.order_id_gen.to_string();
                            self.schedule(self.now + order_delay, Action::OrderArrive(o, id));
                        }
                    }
                    OutcomeEvent::CancelOrder { exchange, order_id, .. } => {
                        self.schedule(
                            self.now + order_delay,
                            Action::CancelArrive(exchange, order_id),
                        );
                    }
                    // Emit 已随上面的 outcome 观察者送达（等价实盘 Outcome 总线的外部
                    // 订阅者），此处无事可做 —— **绝不回流给策略**，策略间不能直接通信。
                    OutcomeEvent::Emit(_) => {}
                }
            }
        }
    }

    fn account_info_event(&self, exchange: Exchange, ts: Timestamp) -> IncomeEvent {
        let state = self.states.get(&exchange).expect("柜台已创建");
        let equity = state.ledger.equity(|s: &Symbol| state.mark_of(s));
        let notional = state.ledger.notional(|s: &Symbol| state.mark_of(s));
        IncomeEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: ExchangeEventData::AccountInfo {
                exchange,
                equity,
                notional,
            },
        }
    }
}

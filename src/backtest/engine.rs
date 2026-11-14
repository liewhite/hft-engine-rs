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
//! 账户净值：周期性 (clock_interval_ms) 由当前账本计算净值（Equity）投递给策略 (等价实盘的
//! accountRefresh)，并在首个事件时先投递一次初始净值，否则依赖净值的策略不会动作。

use crate::backtest::source::MarketDataSource;
use crate::domain::{Exchange, Order, OrderId, Symbol, SymbolMeta, Timestamp};
use crate::engine::StrategyRunner;
use crate::messaging::{AccountData, ExchangeAccountData, IncomeEvent, SymbolAccountData};
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
    /// 持仓均价：记账派生量（非可下单价格），与账本 `BookSnapshot` 同为 f64
    pub entry_price: f64,
    pub unrealized_pnl: f64,
}

/// 回测结果汇总。资金按所分开给（各所现金币种 = 本所计价币，不同所不相加，#841）。
#[derive(Debug, Clone, PartialEq)]
pub struct BacktestResult {
    /// 各所柜台的资金结果，按所排序；没有任何行情时为空
    pub accounts: Vec<BacktestAccount>,
    pub fills: u64,
    /// 回放的**行情**事件数。数据源合成的账户事件（BsGreeksSource 的 Greeks/Balance）
    /// 不计入 —— 那不是行情，计进来会按合成频率成倍放大这个数字。
    pub market_events: u64,
    /// 非空持仓 (按 symbol 升序，保证确定性比对)
    pub positions: Vec<BacktestPosition>,
    pub first_ts: Timestamp,
    pub last_ts: Timestamp,
}

/// 一个所柜台的资金结果，金额都以 `asset`（本所计价币）计
#[derive(Debug, Clone, PartialEq)]
pub struct BacktestAccount {
    pub exchange: Exchange,
    pub asset: String,
    pub initial_balance: f64,
    /// 含未实现盈亏
    pub final_equity: f64,
    /// 已实现盈亏（= 账本现金增量）
    pub realized_pnl: f64,
}

/// 旁路观察者：与策略共享同一事件流但不参与撮合 (如成交记录器)。
type Observer<'a> = Box<dyn FnMut(&IncomeEvent) + 'a>;

/// 队列内的延迟动作。
enum Action {
    /// 交易所侧事件到达策略/观察者
    Deliver(IncomeEvent),
    OrderArrive(Order, OrderId),
    /// 按客户端订单号撤单
    CancelArrive(Exchange, Symbol, String),
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
/// 到达时惰性创建、各以 `initial_balance` 起步（本所计价币计，等价"每个所各入金一份"）。行情按
/// 事件自带的 exchange 送进对应柜台 —— 此前引擎持有单个 exchange 字段并忽略事件自身的
/// 归属，跨所策略（如 funding_arb）无法回测。`BTreeMap` 保证遍历顺序确定（净值事件的
/// 投递序影响确定性）。
pub struct BacktestEngine<'a> {
    source: &'a dyn MarketDataSource,
    runners: Vec<StrategyRunner>,
    config: SimConfig,
    /// 市场规则来源，透传给各所柜台（[`SimState`] 在订单到达时校验下界并取整 ——
    /// 保证实盘必拒的单回测也拒）。调用方**应**与 `StrategyRunner` 传同一张表，
    /// 否则策略侧取整与柜台校验会按两套规则各行其是（bin 侧统一构建一份 Arc）。
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 各所计价币 = 该所柜台的现金币种（见 [`crate::sim::exchange_quotes`]）
    cash_assets: HashMap<Exchange, String>,
    observers: Vec<Observer<'a>>,
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
    /// 某个策略的订单跟踪表报错（见 [`StrategyRunner::on_event`]）：回测就此中止
    failure: Option<String>,
}

impl<'a> BacktestEngine<'a> {
    pub fn new(
        source: &'a dyn MarketDataSource,
        runners: Vec<StrategyRunner>,
        config: SimConfig,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    ) -> Self {
        // 回测的私有事件一律标 backtest_account()；runner 绑别的账户会静默收不到任何成交，结果全错
        for r in &runners {
            assert_eq!(
                r.account(),
                &super::backtest_account(),
                "回测 runner 必须绑定 backtest_account()，否则收不到回测撮合的私有事件"
            );
        }
        let cash_assets = crate::sim::exchange_quotes(&symbol_metas)
            .unwrap_or_else(|e| panic!("回测的合约规格不合用: {e}"));
        Self {
            source,
            runners,
            config,
            symbol_metas,
            cash_assets,
            observers: Vec::new(),
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
            failure: None,
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

    /// 跑完整个数据源，返回汇总结果。`Err` = 某个策略的订单跟踪表说不清挂单与持仓
    /// （等价实盘的停机，见 [`StrategyRunner::on_event`]），回测中止。
    pub fn run(&mut self) -> Result<BacktestResult, String> {
        let mut src = self.source.events().peekable();
        let first = match src.peek() {
            Some(ev) => ev.exchange_ts(),
            None => {
                tracing::warn!("no market data; empty backtest");
                return Ok(BacktestResult {
                    accounts: Vec::new(),
                    fills: 0,
                    market_events: 0,
                    positions: Vec::new(),
                    first_ts: 0,
                    last_ts: 0,
                });
            }
        };
        self.first_ts = first;
        self.now = first;
        // 初始净值随各所柜台的惰性创建投递（见 ensure_state），让依赖 equity 的策略可启动
        self.schedule(self.now + self.clock_interval_ms, Action::Clock);

        while src.peek().is_some() || !self.pq.is_empty() {
            if let Some(e) = self.failure.take() {
                return Err(e);
            }
            self.more_data = src.peek().is_some();
            let src_ts = src.peek().map(|e| e.exchange_ts());
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
        if let Some(e) = self.failure.take() {
            return Err(e);
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
        // 每个所各入金一份，各按本所计价币结算，不相加
        let initial_balance = self.config.initial_balance;
        let accounts = self
            .states
            .iter()
            .map(|(exchange, state)| BacktestAccount {
                exchange: *exchange,
                asset: state.cash_asset.clone(),
                initial_balance,
                final_equity: state.ledger.equity(|s| state.mark_of(s)),
                realized_pnl: state.ledger.cash - initial_balance,
            })
            .collect();
        let result = BacktestResult {
            accounts,
            fills: self.fill_count,
            market_events: self.market_events,
            positions,
            first_ts: self.first_ts,
            last_ts: self.now,
        };
        tracing::info!(
            events = result.market_events,
            fills = result.fills,
            accounts = ?result.accounts,
            "backtest done"
        );
        Ok(result)
    }

    /// 注入一条历史源事件：行情按事件自身的 exchange 路由到对应柜台撮合，行情本身与
    /// 撮合回报都按入向延迟投递；账户事件（如 BS 合成的 Greeks/Balance）不参与撮合，
    /// 直接按入向延迟投递。
    fn ingest(&mut self, ev: IncomeEvent) {
        self.now = ev.exchange_ts();
        let market = match &ev {
            IncomeEvent::Market(m) => m,
            IncomeEvent::Account(_) => {
                // **不计入 market_events**：数据源合成的账户事件（如 BsGreeksSource 每秒
                // 一条 Greeks）不是行情。计进去会把"回放了多少行情"这个数字按合成频率
                // 成倍放大 —— 30 天回测里 260 万条合成 Greeks 会把真实成交笔数淹掉。
                let delay = self.config.exchange_to_strategy_delay_ms;
                self.schedule(self.now + delay, Action::Deliver(ev));
                return;
            }
        };
        self.market_events += 1;
        let Some(exchange) = market.data.exchange() else {
            // 无交易所归属的行情（如无 scope 的 Custom）不参与撮合，按入向延迟投递
            let delay = self.config.exchange_to_strategy_delay_ms;
            self.schedule(self.now + delay, Action::Deliver(ev));
            return;
        };
        self.ensure_state(exchange);
        let replies = self
            .states
            .get_mut(&exchange)
            .expect("ensure_state 刚创建")
            .on_market(market);
        // 行情本身按入向延迟转发给策略（柜台只产回报、不回显行情）
        let delay = self.config.exchange_to_strategy_delay_ms;
        self.schedule(self.now + delay, Action::Deliver(ev));
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
                // 只为有行情的所建柜台，行情的 (所, 币) 都在合约规格里
                self.cash_assets
                    .get(&exchange)
                    .cloned()
                    .unwrap_or_else(|| panic!("{exchange} 没有合约规格，不该有它的回测柜台")),
                self.config.initial_balance,
                self.config.maker_fee_rate,
                self.config.taker_fee_rate,
                self.symbol_metas.clone(),
            ),
        );
        for ev in self.account_events(exchange, self.now) {
            self.schedule(self.now, Action::Deliver(ev));
        }
    }

    /// 把撮合回报按 ex->strat 延迟入队投递给策略（统一归属回测账户）。
    fn enqueue_replies(&mut self, replies: Vec<(Timestamp, AccountData)>) {
        let delay = self.config.exchange_to_strategy_delay_ms;
        for (ts, data) in replies {
            let ev = IncomeEvent::account(crate::backtest::backtest_account(), ts, ts, data);
            self.schedule(self.now + delay, Action::Deliver(ev));
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
            Action::CancelArrive(exchange, symbol, client_order_id) => {
                self.ensure_state(exchange);
                let replies = self
                    .states
                    .get_mut(&exchange)
                    .expect("ensure_state 刚创建")
                    .on_cancel_arrived(self.now, &symbol, &client_order_id);
                self.enqueue_replies(replies);
            }
            Action::Clock => {
                let clock =
                    IncomeEvent::market(self.now, self.now, crate::messaging::MarketData::Clock);
                self.deliver(clock);
                // 周期刷新各所净值, 等价实盘 accountRefresh（BTreeMap 保证投递序确定）
                let exchanges: Vec<Exchange> = self.states.keys().copied().collect();
                for exchange in exchanges {
                    for ev in self.account_events(exchange, self.now) {
                        self.deliver(ev);
                    }
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
        if matches!(&ev, IncomeEvent::Account(a) if matches!(a.data, AccountData::Symbol(SymbolAccountData::Fill(_))))
        {
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
            let outcomes = match self.runners[i].on_event(&ev) {
                Ok(outcomes) => outcomes,
                Err(e) => {
                    tracing::error!(runner = i, error = %e, "策略订单跟踪表报错，回测中止");
                    self.failure.get_or_insert(e);
                    return;
                }
            };
            for outcome in outcomes {
                match outcome {
                    OutcomeEvent::PlaceOrders { orders, .. } => {
                        for o in orders {
                            self.order_id_gen += 1;
                            let id = self.order_id_gen.to_string();
                            self.schedule(self.now + order_delay, Action::OrderArrive(o, id));
                        }
                    }
                    OutcomeEvent::CancelOrder {
                        exchange,
                        symbol,
                        client_order_id,
                    } => {
                        self.schedule(
                            self.now + order_delay,
                            Action::CancelArrive(exchange, symbol, client_order_id),
                        );
                    }
                }
            }
        }
    }

    /// 所级账户读数：净值（柜台现金币种）+ 各币持仓价值（与实盘账本同一读数，#759）
    fn account_events(&self, exchange: Exchange, ts: Timestamp) -> [IncomeEvent; 2] {
        let state = self.states.get(&exchange).expect("柜台已创建");
        let mark_of = |s: &Symbol| state.mark_of(s);
        let info = ExchangeAccountData::Equity {
            exchange,
            equity: crate::domain::Money::new(
                state.ledger.equity(mark_of),
                state.cash_asset.clone(),
            ),
        };
        let values = ExchangeAccountData::PositionValues(crate::messaging::PositionValues {
            exchange,
            legs: state.ledger.position_values(mark_of),
        });
        [info, values].map(|d| {
            IncomeEvent::account(
                crate::backtest::backtest_account(),
                ts,
                ts,
                AccountData::Exchange(d),
            )
        })
    }
}

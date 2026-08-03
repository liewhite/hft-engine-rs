//! SupervisorActor —— 按 symbol 的晋升 / 降级调度。
//!
//! 每个 symbol 常驻一个模拟账户；判据认为可以时拉起该 symbol 的实盘策略实例，判据认为该收时
//! 撤下实例并立即平掉实盘仓位。判据本身是扩展点，见 [`PromotionPolicy`]。
//!
//! ```text
//!            共享行情 ──> 模拟策略实例 ──> 本地柜台 ──> 模拟账户成交
//!                    └──> 实盘策略实例 ──> 交易所   ──> 实盘账户成交
//!                                                           │
//!                            两边成交都汇到 Supervisor ──────┘
//!                                     │
//!                              PromotionPolicy 决定开/关实盘
//! ```
//!
//! # 职责边界
//!
//! Supervisor 只做三件事：**记录表现**、**按节拍询问判据**、**执行决定**。它不含任何阈值，
//! 也不参与撮合与下单细节 —— 晋升就是向 ManagerActor 要一个绑定实盘账户的策略实例，降级就是
//! 让它撤下并平仓。

mod policy;
mod record;

pub use policy::{Decision, NeverPromote, PromotionPolicy, SymbolView};
pub use record::{RoundTrip, SymbolRecord};

use crate::domain::{AccountId, Exchange, Symbol, Timestamp};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::Strategy;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;

use super::{AccountIncome, AddStrategies, ManagerActor, RemoveStrategies, StrategySpec};

/// 每 symbol 保留多少次往返供判据使用
const DEFAULT_ROUND_TRIP_CAPACITY: usize = 512;

/// 为某个 symbol 造一个策略实例。
///
/// 模拟与实盘用**同一个工厂**：两边必须是同一份逻辑、同一组参数，否则模拟的结论无法外推到
/// 实盘。工厂只按 symbol 生成，不接收账户 —— 账户由 [`StrategySpec`] 绑定，策略本身不需要
/// （也不应该）知道自己跑在哪个账户上。
pub type StrategyFactory = Box<dyn Fn(&Symbol) -> Box<dyn Strategy> + Send + 'static>;

/// SupervisorActor 初始化参数
pub struct SupervisorArgs {
    /// 纳入调度的 symbol（本项目按成交额取 Top N）
    pub symbols: Vec<Symbol>,
    /// 交易所（实盘下单去哪、降级平仓去哪）
    pub exchange: Exchange,
    /// 策略工厂：模拟与实盘共用
    pub strategy_factory: StrategyFactory,
    /// 晋升 / 降级判据
    pub policy: Box<dyn PromotionPolicy>,
    /// ManagerActor：晋升/降级通过它增删策略实例
    pub manager: ActorRef<ManagerActor>,
}

/// 单 symbol 的调度状态
struct SymbolState {
    paper: SymbolRecord,
    /// 实盘记录；`None` = 当前未开实盘
    live: Option<SymbolRecord>,
    live_since: Option<Timestamp>,
    /// 晋升/降级请求在途：避免同一 symbol 连续下达重复指令
    transition_in_flight: bool,
}

impl SymbolState {
    fn new(symbol: &Symbol) -> Self {
        Self {
            paper: SymbolRecord::new(symbol.clone(), DEFAULT_ROUND_TRIP_CAPACITY),
            live: None,
            live_since: None,
            transition_in_flight: false,
        }
    }
}

pub struct SupervisorActor {
    symbols: Vec<Symbol>,
    exchange: Exchange,
    states: HashMap<Symbol, SymbolState>,
    strategy_factory: StrategyFactory,
    policy: Box<dyn PromotionPolicy>,
    manager: ActorRef<ManagerActor>,
}

impl SupervisorActor {
    /// 模拟账户的标签约定：按 symbol 分账，使每个 symbol 的盈亏彼此独立
    fn paper_account(symbol: &Symbol) -> AccountId {
        AccountId::Paper(symbol.clone())
    }

    /// 启动期把所有 symbol 的模拟实例挂上去（实盘实例按需再加）
    async fn start_all_paper(&mut self) {
        let specs: Vec<StrategySpec> = self
            .symbols
            .iter()
            .map(|s| StrategySpec {
                strategy: (self.strategy_factory)(s),
                account: Self::paper_account(s),
            })
            .collect();
        if specs.is_empty() {
            tracing::warn!("Supervisor 没有任何 symbol，不会有模拟账户在跑");
            return;
        }
        let count = specs.len();
        match self.manager.ask(AddStrategies(specs)).send().await {
            Ok(()) => tracing::info!(count, "Supervisor started paper instances"),
            Err(e) => tracing::error!(error = %e, "Failed to add paper instances"),
        }
    }

    /// 把一笔成交记到对应账户的表现里
    fn record_fill(&mut self, account: &AccountId, event: &IncomeEvent) {
        let ExchangeEventData::Fill(fill) = &event.data else {
            return;
        };
        let Some(state) = self.states.get_mut(&fill.symbol) else {
            return; // 不在调度范围内的 symbol
        };
        match account {
            AccountId::Paper(_) => {
                state.paper.apply_fill(fill);
            }
            AccountId::Live => {
                if let Some(live) = state.live.as_mut() {
                    live.apply_fill(fill);
                } else {
                    // 已降级但仍有成交回报（平仓单的回报会在撤下之后到达），属正常尾声
                    tracing::debug!(
                        symbol = %fill.symbol,
                        "Live fill after demotion (likely the flattening order)"
                    );
                }
            }
        }
    }

    /// 按节拍逐 symbol 询问判据并执行
    async fn evaluate(&mut self, now: Timestamp) {
        let symbols = self.symbols.clone();
        for symbol in symbols {
            let decision = {
                let Some(state) = self.states.get(&symbol) else {
                    continue;
                };
                if state.transition_in_flight {
                    continue;
                }
                let view = SymbolView {
                    symbol: &symbol,
                    paper: &state.paper,
                    live: state.live.as_ref(),
                    live_since: state.live_since,
                    now,
                };
                self.policy.decide(&view)
            };

            match decision {
                Decision::Hold => {}
                Decision::Promote => self.promote(&symbol, now).await,
                Decision::Demote => self.demote(&symbol).await,
            }
        }
    }

    async fn promote(&mut self, symbol: &Symbol, now: Timestamp) {
        let Some(state) = self.states.get_mut(symbol) else {
            return;
        };
        if state.live.is_some() {
            tracing::warn!(%symbol, "判据在实盘已开启时返回 Promote，已忽略");
            return;
        }
        state.transition_in_flight = true;

        let spec = StrategySpec {
            strategy: (self.strategy_factory)(symbol),
            account: AccountId::Live,
        };
        tracing::warn!(%symbol, "[PROMOTE] 开启实盘 —— 该 symbol 开始真实下单");
        match self.manager.ask(AddStrategies(vec![spec])).send().await {
            Ok(()) => {
                let state = self.states.get_mut(symbol).expect("state exists");
                state.live = Some(SymbolRecord::new(
                    symbol.clone(),
                    DEFAULT_ROUND_TRIP_CAPACITY,
                ));
                state.live_since = Some(now);
                state.transition_in_flight = false;
            }
            Err(e) => {
                tracing::error!(%symbol, error = %e, "晋升失败，保持未开实盘");
                self.states
                    .get_mut(symbol)
                    .expect("state exists")
                    .transition_in_flight = false;
            }
        }
    }

    async fn demote(&mut self, symbol: &Symbol) {
        let Some(state) = self.states.get_mut(symbol) else {
            return;
        };
        if state.live.is_none() {
            tracing::warn!(%symbol, "判据在实盘未开启时返回 Demote，已忽略");
            return;
        }
        state.transition_in_flight = true;
        let realized = state.live.as_ref().map(|r| r.realized_pnl()).unwrap_or(0.0);
        tracing::warn!(%symbol, live_realized_pnl = realized, "[DEMOTE] 关闭实盘并平仓");

        let msg = RemoveStrategies {
            account: AccountId::Live,
            symbols: vec![symbol.clone()],
            exchange: self.exchange,
            flatten: true,
        };
        match self.manager.ask(msg).send().await {
            Ok(()) => {
                let state = self.states.get_mut(symbol).expect("state exists");
                state.live = None;
                state.live_since = None;
                state.transition_in_flight = false;
            }
            Err(e) => {
                // 撤下失败时**保持** live 记账状态：实盘实例可能还在跑，谎报已关闭更危险
                tracing::error!(%symbol, error = %e, "降级失败，实盘可能仍在运行，需人工介入");
                self.states
                    .get_mut(symbol)
                    .expect("state exists")
                    .transition_in_flight = false;
            }
        }
    }
}

impl Actor for SupervisorActor {
    type Args = SupervisorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let states = args
            .symbols
            .iter()
            .map(|s| (s.clone(), SymbolState::new(s)))
            .collect();
        let mut me = Self {
            symbols: args.symbols,
            exchange: args.exchange,
            states,
            strategy_factory: args.strategy_factory,
            policy: args.policy,
            manager: args.manager,
        };
        tracing::info!(
            symbols = me.symbols.len(),
            exchange = %me.exchange,
            "SupervisorActor started"
        );
        me.start_all_paper().await;
        let _ = actor_ref; // 无需自投递：评估由 Clock 事件驱动
        Ok(me)
    }

    async fn on_stop(
        &mut self,
        _r: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("SupervisorActor stopped");
        Ok(())
    }
}

/// 共享总线：实盘账户的成交 + Clock 节拍
impl Message<IncomeEvent> for SupervisorActor {
    type Reply = ();

    async fn handle(&mut self, ev: IncomeEvent, _c: &mut Context<Self, Self::Reply>) {
        match &ev.data {
            ExchangeEventData::Fill(_) => self.record_fill(&AccountId::Live, &ev),
            ExchangeEventData::Clock => self.evaluate(ev.local_ts).await,
            _ => {}
        }
    }
}

/// 模拟账户总线：各模拟账户的成交
impl Message<AccountIncome> for SupervisorActor {
    type Reply = ();

    async fn handle(&mut self, msg: AccountIncome, _c: &mut Context<Self, Self::Reply>) {
        self.record_fill(&msg.account, &msg.event);
    }
}

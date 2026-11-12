//! Supervisor —— 按 symbol 的晋升 / 降级调度。
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
//! 也不参与撮合与下单细节 —— 晋升就是向 manager 要一个绑定实盘账户的策略实例，降级就是
//! 让它撤下并平仓。
//!
//! 它是挂在 manager 消费者组下的外部观察者（[`Supervisor::spawn`]）：出错即整机停机，
//! 停机时与其他消费者一起、在全部生产者之后停。

mod policy;
mod record;

pub use policy::{Decision, NeverPromote, PromotionPolicy, SymbolPerformance};
pub use record::{RoundTrip, SymbolRecord};

use crate::domain::{AccountId, Exchange, Symbol, Timestamp};
use crate::messaging::{SymbolAccountData, AccountData, AccountEvent, MarketData, MarketEvent};
use crate::strategy::Strategy;
use crate::tasks::{ChildHandle, Inbox, Scope, Task};
use std::collections::HashMap;

use super::{ManagerError, ManagerHandle, RemoveStrategies, StrategySpec};

/// 每 symbol 保留多少次往返供判据使用
const DEFAULT_ROUND_TRIP_CAPACITY: usize = 512;

/// 为某个 symbol 造一个策略实例。
///
/// 模拟与实盘用**同一个工厂**：两边必须是同一份逻辑、同一组参数，否则模拟的结论无法外推到
/// 实盘。工厂只按 symbol 生成，不接收账户 —— 账户由 [`StrategySpec`] 绑定，策略本身不需要
/// （也不应该）知道自己跑在哪个账户上。
pub type StrategyFactory = Box<dyn Fn(&Symbol) -> Box<dyn Strategy> + Send + 'static>;

/// [`Supervisor::spawn`] 的参数
pub struct SupervisorArgs {
    /// 纳入调度的 symbol（本项目按成交额取 Top N）
    pub symbols: Vec<Symbol>,
    /// 交易所（实盘下单去哪、降级平仓去哪）
    pub exchange: Exchange,
    /// 策略工厂：模拟与实盘共用
    pub strategy_factory: StrategyFactory,
    /// 晋升 / 降级判据
    pub policy: Box<dyn PromotionPolicy>,
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

/// Supervisor 的收件箱消息
enum SupervisorMsg {
    /// 行情总线：只订 Clock 节拍，驱动评估
    Clock(MarketEvent),
    /// 账户总线：实盘与全部模拟账户的成交（账户由事件自带标签区分）
    Fill(AccountEvent),
}

fn is_clock(e: &MarketEvent) -> bool {
    matches!(e.data, MarketData::Clock)
}

fn is_fill(e: &AccountEvent) -> bool {
    matches!(e.data, AccountData::Symbol(SymbolAccountData::Fill(_)))
}

pub struct Supervisor {
    inbox: Inbox<SupervisorMsg>,
    symbols: Vec<Symbol>,
    exchange: Exchange,
    states: HashMap<Symbol, SymbolState>,
    strategy_factory: StrategyFactory,
    policy: Box<dyn PromotionPolicy>,
    /// 晋升/降级通过它增删策略实例
    manager: ManagerHandle,
}

impl Supervisor {
    /// 挂到 manager 的消费者组下（外部观察者入口），构造时订阅行情总线的 Clock 节拍与账户
    /// 总线的成交，再把所有 symbol 的模拟实例挂上去。返回它在树上的句柄。
    ///
    /// 构造失败（含模拟实例投产失败）→ 返回 `Err`，同时冒到根、整机停机。
    pub async fn spawn(manager: &ManagerHandle, args: SupervisorArgs) -> anyhow::Result<ChildHandle> {
        let manager = manager.clone();
        manager
            .observers()
            .clone()
            .spawn("Supervisor", move |scope: Scope| async move {
                let (_addr, mut inbox) = scope.inbox::<SupervisorMsg>();
                // 先订阅再投产：模拟实例一开跑就可能成交，订阅在前才不会漏掉头几笔
                inbox.subscribe(manager.market_bus(), is_clock, SupervisorMsg::Clock);
                inbox.subscribe(manager.account_bus(), is_fill, SupervisorMsg::Fill);
                let states = args
                    .symbols
                    .iter()
                    .map(|s| (s.clone(), SymbolState::new(s)))
                    .collect();
                let mut me = Self {
                    inbox,
                    symbols: args.symbols,
                    exchange: args.exchange,
                    states,
                    strategy_factory: args.strategy_factory,
                    policy: args.policy,
                    manager,
                };
                tracing::info!(
                    symbols = me.symbols.len(),
                    exchange = %me.exchange,
                    "Supervisor started"
                );
                me.start_all_paper().await?;
                Ok(me)
            })
            .await
    }

    /// 模拟账户的标签约定：按 symbol 分账，使每个 symbol 的盈亏彼此独立
    fn paper_account(symbol: &Symbol) -> AccountId {
        AccountId::Paper(symbol.clone())
    }

    /// 启动期把所有 symbol 的模拟实例挂上去（实盘实例按需再加）。
    ///
    /// 失败必须让 Supervisor **启动失败**（进而进程退出），不能只记日志继续跑：
    /// - 引擎会带着零实例空转 —— 无策略、无对账基线、无人告警；
    /// - 半途失败可能已在对账/观测镜像里注册了范围（rollback 有意不撤），滞留状态
    ///   只有进程退出重来才能出清。
    /// 与 bin 直连路径"投产失败即拒绝启动"的语义对齐。运行期的 `promote` 失败则相反 ——
    /// 判据下个节拍会自动重试，吞掉并等待重试是有意的。
    async fn start_all_paper(&mut self) -> anyhow::Result<()> {
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
            return Ok(());
        }
        let count = specs.len();
        self.manager
            .add_strategies(specs)
            .await
            .map_err(|e| anyhow::anyhow!("Supervisor 启动期投产模拟实例失败: {e}"))?;
        tracing::info!(count, "Supervisor started paper instances");
        Ok(())
    }

    /// 把一笔成交记到对应账户的表现里（账户从事件自带的标签取 —— 一条总线一次订阅，
    /// 实盘与全部模拟账户的成交都经此，替代旧的共享总线 + Paper 总线双订阅）
    fn record_fill(&mut self, event: &AccountEvent) {
        let AccountData::Symbol(SymbolAccountData::Fill(fill)) = &event.data else {
            return;
        };
        let account = &event.account;
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
                let view = SymbolPerformance {
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
        match self.manager.add_strategy(spec).await {
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
                Self::log_failure(&e, symbol, "晋升失败，保持未开实盘");
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
        match self.manager.remove_strategies(msg).await {
            Ok(()) => {
                let state = self.states.get_mut(symbol).expect("state exists");
                state.live = None;
                state.live_since = None;
                state.transition_in_flight = false;
            }
            Err(e) => {
                // 撤下失败时**保持** live 记账状态：实盘实例可能还在跑，谎报已关闭更危险
                Self::log_failure(&e, symbol, "降级失败，实盘可能仍在运行，需人工介入");
                self.states
                    .get_mut(symbol)
                    .expect("state exists")
                    .transition_in_flight = false;
            }
        }
    }

    /// 晋升 / 降级请求失败的记录。manager 已停止收件只可能是整机在停机（见
    /// [`ManagerError::Stopping`]）：那不是这一次调度的失败，记 warn；其余是真失败，记 ERROR。
    fn log_failure(e: &ManagerError, symbol: &Symbol, what: &str) {
        match e {
            ManagerError::Stopping => tracing::warn!(%symbol, error = %e, "{what}（引擎正在停机）"),
            ManagerError::Failed(_) => tracing::error!(%symbol, error = %e, "{what}"),
        }
    }
}

impl Task for Supervisor {
    /// 无需自投递：评估由 Clock 事件驱动
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(msg) = self.inbox.recv().await {
            match msg {
                SupervisorMsg::Clock(ev) => self.evaluate(ev.local_ts).await,
                SupervisorMsg::Fill(ev) => self.record_fill(&ev),
            }
        }
        tracing::info!("Supervisor stopped");
        Ok(())
    }
}

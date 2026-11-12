//! Executor —— 一个策略实例
//!
//! 接收 IncomeEvent（由 [`super::IncomeProcessor`] 按订阅范围转发），委托 [`StrategyRunner`]
//! 运行策略（更新状态 + 转换订单），把产出的 OutcomeEvent 打上账户标签发布到 OutcomeBus。
//! 纯策略逻辑集中在 StrategyRunner，与回测引擎共享同一份实现。
//!
//! 运行期由编排（manager）往 producers 组里 spawn；单独撤下时对 [`Executor::spawn`] 返回的
//! [`ChildHandle`] 调 `stop().await`（会把箱里已收到的事件处理完再退出）。

use crate::domain::{now_ms, AccountId, Exchange, Position, Symbol, SymbolMeta, Timestamp};
use crate::engine::StrategyRunner;
use crate::messaging::IncomeEvent;
use crate::strategy::Strategy;
use crate::tasks::{Addr, ChildHandle, Gone, Group, Inbox, Scope, Task};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::oneshot;

use super::{AccountOutcome, OutcomeBus};

/// 策略消费一条事件时，距适配层收到它的最大容忍时长（见
/// [`Executor::check_pipeline_lag`]）。
///
/// 取 1 秒：正常路径上这个值是几毫秒（三跳进程内收件箱），到秒级只有一个解释 —— 本实例
/// 跟不上事件流。它同时是策略决策的陈旧度上限：spread_arb 的行情新鲜度门是秒级，
/// 拿超过 1 秒的报价去算价差已经没有意义。
const MAX_PIPELINE_LAG_MS: u64 = 1_000;

/// 滞后告警的最小间隔。积压时超限事件成千上万，逐条打日志会让告警本身变成负载。
///
/// 用**时间**节流而非"每 N 条报一次"：后者是相位判断（`count % N == 1`），计数器又不
/// 跨积压期重置 —— 上一次积压停在 1002 条，下一次持续不到 1000 条的积压就整段静默，
/// 漏报与否取决于历史计数的余数。而滞后本就是个持续时长的问题，按时间节流才对得上。
const LAG_WARN_INTERVAL_MS: u64 = 5_000;

/// 时间节流判据（纯函数，便于单测）：距上次告警是否已够久。
///
/// `last_warn_ms == 0` 表示从未告警过 —— 差值必然超阈值，故首条必报。
fn lag_warn_due(now: Timestamp, last_warn_ms: Timestamp) -> bool {
    now.saturating_sub(last_warn_ms) >= LAG_WARN_INTERVAL_MS
}

/// 一个策略实例在进程内的身份：分发层（[`super::IncomeProcessor`]）与持仓账本
/// （[`super::PositionLedger`]）的名单都按它记。进程内唯一、不复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExecutorId(u64);

impl ExecutorId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// 测试用：造一个固定的身份（只拿来当名单键，不对应真实实例）
    #[cfg(test)]
    pub(crate) fn for_test(n: u64) -> Self {
        Self(n)
    }
}

impl std::fmt::Display for ExecutorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// [`Executor::spawn`] 的参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// 本实例绑定的账户：决定订单落到实盘还是某个模拟账户
    pub account: AccountId,
    /// Symbol 元数据 (供 StrategyRunner 按交易所精度取整；单位折算不在此)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 持仓基线（投产握手）：实例**出生即带基线**，在收到任何流事件之前写入 ——
    /// "Fill 早于基线、存量被丢"的竞态在结构上不可能。模拟账户从 0 起算，传空。
    pub baselines: Vec<crate::messaging::PositionBaseline>,
    /// 策略信号的去处
    pub outcome_bus: OutcomeBus,
}

/// Executor 的收件箱消息
pub enum ExecutorMsg {
    /// 一条入向事件（行情或账户，含 Clock），由分发层转发
    Event(IncomeEvent),
    /// 见 [`ExecutorHandle::positions`]
    GetPositions(Vec<Symbol>, oneshot::Sender<Vec<Position>>),
    /// 见 [`ExecutorHandle::seeded_positions`]
    GetSeededPositions(oneshot::Sender<Vec<Position>>),
}

/// 一个策略实例的地址（可克隆）。身份 [`ExecutorId`] 随句柄走。
#[derive(Clone, Debug)]
pub struct ExecutorHandle {
    id: ExecutorId,
    addr: Addr<ExecutorMsg>,
}

impl ExecutorHandle {
    /// 测试用：把一个裸收件箱包装成 executor 地址（分发层测试用它代替真实实例）
    #[cfg(test)]
    pub(crate) fn for_test(addr: Addr<ExecutorMsg>) -> Self {
        Self { id: ExecutorId::next(), addr }
    }

    pub fn id(&self) -> ExecutorId {
        self.id
    }

    /// 投一条入向事件（不等处理）。`Gone` = 实例已停止收件。
    pub fn deliver(&self, event: IncomeEvent) -> Result<(), Gone> {
        self.addr.send(ExecutorMsg::Event(event))
    }

    /// 查询本实例维护的持仓（指定 symbol，返回其在各所的仓位）。
    ///
    /// 用于降级平仓取量，见 [`crate::engine::RemoveStrategies`]。读的是「基线 + Fill」维护的
    /// **权威**持仓，**不打 REST**：
    /// - REST 在本项目里只作对账用途（见 [`super::PositionLedger`]），持仓维护模型不允许它参与；
    /// - 本地值比 REST 快照更**新** —— 后者可能落后于最新到达的 Fill。
    ///
    /// 调用方必须在停掉该 executor **之前**发起查询：停掉之后它就不再消费事件、状态停更
    /// （停掉之后再问会得到 `Gone`）。
    pub async fn positions(&self, symbols: Vec<Symbol>) -> Result<Vec<Position>, Gone> {
        self.addr.ask(|tx| ExecutorMsg::GetPositions(symbols, tx)).await
    }

    /// 查询本实例**基线已写入**的全部腿（策略下单所依据的那份持仓），供持仓账本把它纳入
    /// 对账（见 [`super::PositionLedger`] 模块文档「两条比对」）。
    ///
    /// 只给已 seed 的腿：未 seed 的是「未知」，拿去比必然误判。实盘实例出生即把作用域内每条腿
    /// 都 seed 了（见 `executor_baselines`），所以这就是它的全部持仓。
    pub async fn seeded_positions(&self) -> Result<Vec<Position>, Gone> {
        self.addr.ask(ExecutorMsg::GetSeededPositions).await
    }
}

/// 一个策略实例
pub struct Executor {
    inbox: Inbox<ExecutorMsg>,
    /// 策略执行纯逻辑核心 (状态管理 + 订单转换)
    runner: StrategyRunner,
    /// 本实例绑定的账户
    account: AccountId,
    /// 策略信号的去处
    outcome_bus: OutcomeBus,
    /// 累计消费到的超限滞后事件数（进告警看趋势，见 [`Self::check_pipeline_lag`]）
    lagging_events: u64,
    /// 上次滞后告警的时刻；0 = 从未告警（见 [`lag_warn_due`]）
    last_lag_warn_ms: Timestamp,
}

impl Executor {
    /// 在 `group` 下起一个策略实例，等它构造完成（基线已写入），返回它的句柄（单独撤下用）
    /// 和地址。此刻它还没注册到分发层，收不到任何事件。
    pub async fn spawn(
        group: &Group,
        args: ExecutorArgs,
    ) -> anyhow::Result<(ChildHandle, ExecutorHandle)> {
        let id = ExecutorId::next();
        let name = format!("Executor{id}[{}]", args.account);
        let (child, addr) = group
            .spawn_addressed(name, move |_scope: Scope, inbox| async move {
                let account = args.account;
                let mut runner =
                    StrategyRunner::new(args.strategy, args.symbol_metas, account.clone());
                // 基线先于一切流事件写入（构造完成前收件箱里的消息不会被处理）
                runner.seed_positions(&args.baselines);
                tracing::info!(executor = %id, %account, baselines = args.baselines.len(), "Executor started");
                Ok(Self {
                    inbox,
                    runner,
                    account,
                    outcome_bus: args.outcome_bus,
                    lagging_events: 0,
                    last_lag_warn_ms: 0,
                })
            })
            .await?;
        Ok((child, ExecutorHandle { id, addr }))
    }

    fn handle(&mut self, msg: ExecutorMsg) {
        // 回执发送失败 = 提问方已不再等（超时放弃或自身已退出），没有人需要这份答复
        match msg {
            ExecutorMsg::Event(event) => self.handle_event(event),
            ExecutorMsg::GetPositions(symbols, reply) => {
                let state = self.runner.state();
                let positions = symbols
                    .iter()
                    .filter_map(|symbol| state.symbol_state(symbol))
                    .flat_map(|symbol_state| symbol_state.position_book().all().cloned())
                    .collect();
                let _ = reply.send(positions);
            }
            ExecutorMsg::GetSeededPositions(reply) => {
                let positions = self
                    .runner
                    .state()
                    .symbol_states()
                    .flat_map(|(_, symbol_state)| symbol_state.position_book().seeded_positions())
                    .cloned()
                    .collect();
                let _ = reply.send(positions);
            }
        }
    }

    /// 处理 IncomeEvent，委托 runner 运行策略并发布返回的信号
    fn handle_event(&mut self, event: IncomeEvent) {
        self.check_pipeline_lag(&event);
        for signal in self.runner.on_event(&event) {
            // 发布不会失败；订阅者（出口）停止收件时由总线记 ERROR「事件丢失」
            self.outcome_bus.publish(AccountOutcome {
                account: self.account.clone(),
                event: signal,
            });
        }
    }

    /// 管线滞后自查：策略此刻拿到的这条事件，距适配层收到它已经过了多久。
    ///
    /// # 为什么需要它，以及为什么不是 conflation
    ///
    /// 三条总线都投进订阅者的无界收件箱。消费者跟不上时，后果不是丢事件而是
    /// **积压**：策略排着队消费几秒前的报价做决策，而这件事目前**完全不可见** —— 没有
    /// 日志、没有指标，只有内存悄悄涨。
    ///
    /// 计划里原本想用"状态类事件只保留最新值"（conflation）来解决。**不能这么做**：
    /// `spread_arb` 把 BBO 当**流**消费 —— `EmaCalculator` 每条 BBO 更新一次、
    /// `is_ready()` 数的是 tick 次数。丢掉中间 tick 会让 EMA 的值与预热时机变成**负载的
    /// 函数**：同一段行情，机器忙时和闲时算出不同的 EMA，且与逐 tick 回放的回测必然背离
    /// —— 那正是"回测调好的参数上线即漂移"，本次重构阶段 1 刚花力气消除的东西。
    ///
    /// 所以改为让积压**可见**（与 [`crate::exchange::staleness`] 同一姿态：读数流的问题
    /// 要能被发现，而不是被静默吸收）：超过阈值即 warn（按时间节流，见
    /// [`LAG_WARN_INTERVAL_MS`]），配了告警外送时 WARN 会外送
    /// （见 [`crate::observability::alert`]）。
    ///
    /// 不 kill：偶发的 GC / 调度抖动不该打死引擎，而持续积压会持续告警。
    ///
    /// # 只在实盘路径
    ///
    /// 本组件只用于实盘/模拟盘（回测直接驱动 `StrategyRunner`），故 `local_ts` 必是
    /// 墙钟；回测里它是虚拟时间，拿来减 `now_ms()` 会得到无意义的巨值 —— 这也是判据
    /// 放在这里、而不是放进两条驱动共享的 `StrategyRunner` 的原因。
    fn check_pipeline_lag(&mut self, event: &IncomeEvent) {
        let now = now_ms();
        let lag_ms = now.saturating_sub(event.local_ts());
        if lag_ms <= MAX_PIPELINE_LAG_MS {
            return;
        }
        self.lagging_events += 1;
        if !lag_warn_due(now, self.last_lag_warn_ms) {
            return;
        }
        self.last_lag_warn_ms = now;
        tracing::warn!(
            account = %self.account,
            lag_ms,
            threshold_ms = MAX_PIPELINE_LAG_MS,
            lagging_events = self.lagging_events,
            "策略正在消费积压事件（决策依据已经陈旧），本实例跟不上事件流"
        );
    }
}

impl Task for Executor {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 停止时收件箱先关、再把已收到的事件处理完才返回 None：撤下前已转来的事件不会丢
        while let Some(msg) = self.inbox.recv().await {
            self.handle(msg);
        }
        tracing::info!(account = %self.account, "Executor stopped");
        Ok(())
    }
}

#[cfg(test)]
mod lag_throttle_tests {
    use super::*;

    /// 从未告警过 -> 首条必报
    #[test]
    fn first_lagging_event_always_warns() {
        assert!(lag_warn_due(1_700_000_000_000, 0));
    }

    /// 间隔内不重复报（积压时超限事件成千上万，告警自己会变成负载）
    #[test]
    fn within_interval_stays_quiet() {
        let last = 1_700_000_000_000;
        assert!(!lag_warn_due(last + LAG_WARN_INTERVAL_MS - 1, last));
        assert!(lag_warn_due(last + LAG_WARN_INTERVAL_MS, last));
    }

    /// **回归防线**：第二段积压的首条必须能报出来。
    ///
    /// 此前用"每 N 条报一次"的相位判据（`count % N == 1`）且计数器不跨积压期重置：
    /// 上一段积压停在 1002 条，下一段只要持续不到 1000 条，区间内就没有 ≡1 (mod N) 的
    /// 点 —— 整段静默，漏报与否取决于历史计数的余数。改按时间节流后，只要距上次告警
    /// 够久就一定报，与历史条数无关。
    #[test]
    fn a_later_burst_is_never_silenced_by_history() {
        let first_burst_warn_at = 1_700_000_000_000;
        // 中间恢复正常一分钟，随后第二段积压 —— 无论此前报过多少条，都该报
        let second_burst = first_burst_warn_at + 60_000;
        assert!(lag_warn_due(second_burst, first_burst_warn_at));
    }
}

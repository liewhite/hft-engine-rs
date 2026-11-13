//! PositionLedger —— 实盘持仓账本：维护真值、用 REST 校验、对外发读数。
//!
//! # 三件事是同一件事
//!
//! 本组件持有的镜像是**进程内唯一被持续校验的持仓折叠**，因此也是唯一有资格对外称作
//! 「真值」的那一份（策略实例自己那份决策用折叠及其保留理由见 `docs/external-data-access.md`）。
//! 校验与对外不是两个职责：账本没有守卫就是负债，守卫没有账本无物可守，而外部读到的数字
//! 与风控守卫看到的数字**必须是同一个**，否则看板绿着、引擎却在停机边缘。
//!
//! # 各所持仓读数（#759，#812 起是对外的唯一出口）
//!
//! 每笔成交后发该所、每个节拍发全部所的 [`PositionValues`]（币 → 持仓 + 盘口中间价）到账户总线。
//! 策略的账户杠杆闸门拿它加总别的币、再换上自己这一刻的持仓 —— 别的币刚成交，几毫秒内就能看到，
//! 不用等交易所账户接口的轮询；指标（看板上的持仓、杠杆、会话盈亏）也只读它，不自己折叠、自己估值。
//! 盘口只为估值而订（行情总线上的 BBO）；有仓的币还没有盘口时不发（未知不当 0）。
//!
//! 持仓的权威值由「投产期 REST 基线 + 之后全程 Fill 累加」维护
//! （见 [`crate::messaging::PositionBaseline`]）。这条通道会错：Fill 可能丢、可能被重复
//! 计算、codec 可能算错单位或符号、也可能出现不伴随 Fill 的交易所侧变动。本组件用**独立
//! 的第二条通道** —— 周期性 REST 读数 —— 校验第一条：
//!
//! ```text
//!   通道 A（权威）  基线 ──> Fill 累加 ──> 策略看到的持仓
//!                                │
//!   通道 B（校验）  自行轮询 REST 读数 ──> 比对 ──> 连续 N 次差值稳定的不一致即致命退出
//! ```
//!
//! # 读数自产自销：轮询就在本组件里
//!
//! 读数（某所的**完整**持仓快照）只有一个消费者 —— 本组件。它不是事件，不上总线：
//! 本组件持有各所的 `AccountClient`，按节拍并发拉取、就地比对。完整快照的意义在于
//! "交易所没报告这个 symbol ⇒ 它空仓"这个推断 —— 对账最要抓的一类漂移正是**本地有仓、
//! 交易所已经没了**（漏了平仓成交/强平回报），只有全量快照能发现它。
//!
//! # 职责边界：为什么不放进观测组件（Metrics）
//!
//! 观测组件明确声明"观测组件故障不得拖垮交易"，连定时器挂掉都不让自己停。而对账
//! 连续失败要**致命退出**，那是风控语义而非观测语义。混在一起会破坏它已经写明的边界。
//! 同理，持仓查询也不挂在它上面：让观测组件当权威数据源会反转这条边界。
//!
//! # 拉取不阻塞收件箱
//!
//! 节拍到达时为各所发起拉取，放进 `run` 自己持有的一组在途 future，与收件箱在同一个
//! `select!` 里并发推进；结果到达后就地比对。**收件箱在任何时刻都不阻塞在 IO 上**，这既是
//! 快照面契约的硬性要求，也修掉了一个既有缺陷：此前处理节拍时 `join_all` 各所 REST，最长
//! 一个 client 超时期间**本轮比对与 Fill 摄入一起停摆**。比对仍串行发生在本任务内部，镜像
//! 不需要任何锁。
//!
//! 在途 future 由 `run` 自己 poll，结果不经消息回投：**不存在"结果永不回投"的路径**；拉取
//! panic 随 `run` 一起上冒（整支停、错误冒到根）。剩下唯一的"迟迟不返回"是请求本身挂死，
//! 由在途停摆判据兜住（见 `Ledger::check_in_flight_staleness`）。
//!
//! # 两条比对：账本 vs 交易所，executor vs 账本
//!
//! 本组件自建一份镜像（[`PositionBook`]，与策略侧同一份 seed 语义与防双计规则），由投产
//! 握手的基线 + 账户总线的实盘 `Fill` 驱动。对交易所读数比对证明的是"**事件流**与交易所
//! 一致"。
//!
//! 但策略下单用的是 executor 自己那份折叠，它经另一条分发路径（IncomeProcessor 按订阅
//! 转发）拿 Fill —— 分发出错、或同一 symbol 再次投产时 executor 用了更新的快照 seed，它会单独错，而上面
//! 那条比对看不见。所以每个节拍也向每个**实盘** executor 要一份它的持仓，与账本按同一判据
//! 比对（#694）：
//!
//! ```text
//!   交易所 REST ──比对──> 账本镜像 <──比对── 实盘 executor 的持仓（下单依据）
//! ```
//!
//! 监视名单由编排维护：投产时 [`PositionLedgerHandle::watch`]，撤下/回滚时先
//! [`PositionLedgerHandle::unwatch`] 再注销订阅 —— 顺序要紧，注销后的 executor 不再收 Fill，
//! 若还在名单里会被误判分叉。模拟账户的 executor 不进名单（它的持仓本就与实盘无关）。
//!
//! # 注册与基线原子到达，无缓冲重放
//!
//! [`PositionLedgerHandle::register_symbols`] 一次携带 symbol 集合与它们的基线（与 executor
//! 同一份快照）。"已注册、基线未到"的窗口不存在，历史上为它建的 Fill 缓冲 + 按快照时刻过滤
//! 重放的机制随之删除；快照与 Fill 流的竞态由 seed 内置的时刻过滤统一兜住。

use super::executor::{ExecutorHandle, ExecutorId};
use crate::domain::{now_ms, AccountId};
use crate::domain::{Exchange, Position, Price, Quantity, Symbol};
use crate::exchange::staleness::{StalenessGuard, MAX_POLL_STALENESS_MS};
use crate::exchange::AccountClient;
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, LegValue, MarketBus, MarketData,
    MarketEvent, PositionBaseline, PositionBook, PositionValues,
};
use crate::tasks::{Addr, ChildHandle, Gone, Group, Inbox, Scope, StopFlag, Task};
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

/// 持仓对账的轮询间隔。
///
/// 与"连续 N 次不一致才致命"配合决定反应速度：3s × 3 次 = 持续约 9 秒不一致才停机。
/// 间隔不能太短——它同时是"飞行窗口"的宽限期：REST 快照时点与本地 Fill 流之间必然有
/// 间隙，间隔太短会让正常成交被误判成漂移。
pub const DEFAULT_POSITION_POLL_INTERVAL_MS: u64 = 3_000;

/// 连续多少次**差值稳定**的不一致才判定为真实漂移，见 [`MismatchRecord`]。
///
/// 不能取 1：REST 快照的时点与本地 Fill 流之间必然有间隙（成交已落交易所、Fill 稍后才到），
/// 单次不一致是正常的飞行窗口。
pub const DEFAULT_MAX_CONSECUTIVE_MISMATCHES: u32 = 3;

// ============================================================================
// 纯逻辑核心
// ============================================================================

/// 对账纯逻辑：注册基线、跟 Fill、比对读数。不含任何任务 / 时钟 / IO，可同步单测。
///
/// 与 [`crate::engine::StrategyRunner`] 同样的分层：纯核心 + 薄组件包装。
pub struct Reconciler {
    /// 持仓账本：投产握手 seed 基线 + income 流的 `Fill` 驱动。
    ///
    /// 类型就是职责声明 —— 它只有持仓。此前这里是整个 `StateManager`（行情缓存、挂单
    /// 跟踪、账户净值、希腊值一个都不碰却全都拿着），见 `docs/architecture.md` 原则 P3。
    ///
    /// 同时充当两个判据的单一数据源，不另存副本：
    /// - "本实例负责哪些 symbol"：`is_tracked`。分桶部署下多实例共用同一账户，
    ///   交易所读数是**账户级全量**、含其他桶的 symbol，靠这个判据过滤。
    /// - "哪条腿可参与对账"：`is_seeded` —— 基线未到（该所未配置凭证、无账户）时
    ///   本地持仓是"未知"而不是 0，拿未知去比对必然误判。
    mirror: PositionBook,
    /// per (对手方, 所, symbol) 的不一致记录；一致一次即整条移除
    mismatches: HashMap<(Counterpart, Exchange, Symbol), MismatchRecord>,
    max_consecutive: u32,
}

/// 账本的一条腿拿去和谁比。两种对手方用**同一判据**（差值连续稳定 N 次即致命），
/// 只是各自独立计数。
///
/// - [`Counterpart::Exchange`]：交易所 REST 读数 —— 证明「事件流」与交易所一致；
/// - [`Counterpart::Executor`]：某个实盘 executor 的决策用持仓 —— 证明「下单依据的那份」
///   与事件流一致。两份都是同一条 Fill 流的确定性折叠，流对了它本该就对，但分发出错
///   （某条 Fill 没送到某个 executor、同一 symbol 再次投产时 executor 用了新快照）会让它单独错，
///   而那一份才是真正决定下单量的。两条比对合起来：executor ≈ 账本 ≈ 交易所。
///
/// 为什么 executor 比账本、而不是直接比 REST：账本是权威，拿派生去比权威是「派生是否
/// 失真」最直接的问法；且两者都在进程内，不必和某所某次 REST 读数的时点配对。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Counterpart {
    Exchange,
    Executor(ExecutorId),
}

impl std::fmt::Display for Counterpart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exchange => write!(f, "交易所"),
            Self::Executor(id) => write!(f, "executor {id}"),
        }
    }
}

/// 某条腿的连续不一致记录。
///
/// # 为什么要记住差值，而不是只数次数
///
/// 只数次数会把**在途成交**误判成漂移：某 symbol 若持续成交，每次轮询都可能正好撞在
/// "成交已落交易所、Fill 尚未送达"的窗口上，攒够次数就误停机 —— 而误停机比漏报更糟。
///
/// 差值本身能区分这两件事：
/// - **真漂移**（漏算了一笔 X）→ 差值恒为 X。此后即使继续交易，本地与交易所同步移动，
///   差值仍是 X，不会被"抹平"
/// - **在途成交** → 差值等于当时在途的量，每次轮询都不一样
///
/// 故只在"差值与上次相同"时累加，差值一变就重新从 1 数起。这比单纯提高次数阈值更准：
/// 既不误伤飞行窗口，也不会在活跃交易期压制真实漂移的检出。
#[derive(Debug, Clone, Copy)]
struct MismatchRecord {
    /// 连续**同一差值**的不一致次数
    streak: u32,
    /// 上一次的**带符号**差值 `local - reported`（带符号：+X 与 -X 是不同的漂移）
    last_diff: Quantity,
}

impl Reconciler {
    pub fn new(max_consecutive: u32) -> Self {
        Self {
            mirror: PositionBook::default(),
            mismatches: HashMap::new(),
            max_consecutive,
        }
    }

    /// 注册要对账的 symbol 并**原子**写入它们的持仓基线（投产握手）。
    ///
    /// 与 executor / 观测镜像同一份快照、同一 seed 语义（幂等，重复注册是同一 symbol 再次投产的
    /// 正常形态）；快照与 Fill 流的竞态由 seed 内置的时刻过滤兜住。
    pub fn register(&mut self, symbols: &[Symbol], baselines: &[PositionBaseline]) {
        self.mirror.register_symbols(symbols);
        self.mirror.seed(baselines);
    }

    /// 喂入一条账户事件（账本只吃**实盘**的 Fill、且只吃跟踪范围内的 —— 判据与丢弃
    /// 语义都在 [`PositionBook::apply`] 里，本层不再重复一遍）。
    pub fn on_event(&mut self, event: &AccountEvent) {
        self.mirror.apply(event);
    }

    /// 该所每个基线已写入的币的持仓读数：数量 + `mid_of` 给的估值价（#759，#812 起带数量）。
    ///
    /// 有仓的币拿不到价时返回 `Err(那个币)`：价值未知，不能当 0 给出去（杠杆闸门会少算）。
    /// 空仓的币不需要价（有就带上，指标拿它给会话盈亏估值）。
    pub fn position_values(
        &self,
        exchange: Exchange,
        mid_of: impl Fn(&Symbol) -> Option<Price>,
    ) -> Result<HashMap<Symbol, LegValue>, Symbol> {
        self.mirror
            .iter()
            .filter(|(_, state)| state.is_seeded(exchange))
            .map(|(symbol, state)| {
                LegValue::new(state.position_size(exchange), mid_of(symbol))
                    .map(|leg| (symbol.clone(), leg))
                    .ok_or_else(|| symbol.clone())
            })
            .collect()
    }

    /// 比对某所的一份完整读数。
    ///
    /// `Err` = 已确认漂移（连续 `max_consecutive` 次差值稳定的不一致），或读数里同一 symbol
    /// 出现两条（映射混了合约，见 `index_positions`），调用方应致命退出。
    pub fn reconcile(&mut self, exchange: Exchange, positions: &[Position]) -> Result<(), String> {
        // 读数是该所的**完整**快照，因此"跟踪范围内、但读数里没有的 symbol"= 交易所侧空仓。
        // 这个推断正是选 REST 全量而非私有 WS 增量推送的理由（见模块文档）：
        // 它才能抓到最危险的一类漂移 —— 本地有仓、交易所已经没了。
        // 同一 symbol 两条 = 映射混了合约，读数不可信，按致命处理（见 index_positions）
        let reported = crate::exchange::client::index_positions(positions.to_vec())?;

        // 只比对**该所**基线已写入的腿（基线未到 = 本地未知，不是 0）
        let legs: Vec<(Symbol, Quantity, Quantity)> = self
            .mirror
            .iter()
            .filter(|(_, state)| state.is_seeded(exchange))
            .map(|(symbol, state)| {
                let reported_size = reported.get(symbol).map_or(Quantity::ZERO, |p| p.size);
                (symbol.clone(), state.position_size(exchange), reported_size)
            })
            .collect();
        let confirmed: Vec<String> = legs
            .into_iter()
            .filter_map(|(symbol, local, reported)| {
                self.check_leg(Counterpart::Exchange, exchange, symbol, local, reported)
            })
            .collect();
        self.verdict(Counterpart::Exchange, confirmed)
    }

    /// 比对某个实盘 executor 的决策用持仓（它的**基线已写入**的全部腿）与账本。
    ///
    /// 只比两边都有基线的腿。实盘策略作用域内的所投产时已校验配了凭证、每条腿都有快照（见
    /// `AccountBinding::into_baselines`，#767），账本未 seed 的腿正常不会出现。账本有、executor 作用域外的腿
    /// 不归它管，也不比。
    ///
    /// `Err` = 已确认该 executor 与账本分叉（连续 `max_consecutive` 次差值稳定），调用方应
    /// 致命退出 —— 策略正拿着错的持仓算下单量。
    pub fn reconcile_executor(
        &mut self,
        executor: ExecutorId,
        positions: &[Position],
    ) -> Result<(), String> {
        let counterpart = Counterpart::Executor(executor);
        let legs: Vec<(Exchange, Symbol, Quantity, Quantity)> = positions
            .iter()
            .filter_map(|p| {
                let state = self.mirror.get(&p.symbol)?;
                state.is_seeded(p.exchange).then(|| {
                    (
                        p.exchange,
                        p.symbol.clone(),
                        state.position_size(p.exchange),
                        p.size,
                    )
                })
            })
            .collect();
        let confirmed: Vec<String> = legs
            .into_iter()
            .filter_map(|(exchange, symbol, local, reported)| {
                self.check_leg(counterpart, exchange, symbol, local, reported)
            })
            .collect();
        self.verdict(counterpart, confirmed)
    }

    /// 一条腿的比对与连续计数（两种对手方共用的**唯一**判据）。
    ///
    /// `local` 是账本值，`reported` 是对手方的值。返回 `Some(描述)` = 该腿已确认漂移。
    fn check_leg(
        &mut self,
        counterpart: Counterpart,
        exchange: Exchange,
        symbol: Symbol,
        local: Quantity,
        reported: Quantity,
    ) -> Option<String> {
        // 带符号差值：+X 与 -X 是方向相反的两种漂移，不能混为一谈
        let diff = local - reported;
        let key = (counterpart, exchange, symbol);

        if diff.is_zero() {
            // 一致即整条移除：记录衡量的是"当前正持续着的不一致"，不是历史累计
            self.mismatches.remove(&key);
            return None;
        }

        // 差值是否与上次相同 —— 区分真漂移与在途成交，见 MismatchRecord
        let prev = self.mismatches.get(&key).copied();
        let stable = prev.is_some_and(|prev| diff == prev.last_diff);
        let streak = match prev {
            Some(prev) if stable => prev.streak + 1,
            _ => 1,
        };
        let (_, _, symbol) = &key;

        // 打成结构化日志，让漂移成为可观测的趋势，而不是只有停机那一刻才被看到
        tracing::warn!(
            target: "metrics",
            %counterpart,
            %exchange,
            %symbol,
            local_size = %local,
            reported_size = %reported,
            diff = %diff,
            stable_diff = stable,
            streak,
            max_consecutive = self.max_consecutive,
            "metrics.position_drift：本地持仓与{counterpart}不一致"
        );

        let confirmed = (streak >= self.max_consecutive).then(|| {
            format!(
                "{exchange}/{symbol}: 本地 {local} vs {counterpart} {reported} \
                 (差 {diff})"
            )
        });
        self.mismatches.insert(
            key,
            MismatchRecord {
                streak,
                last_diff: diff,
            },
        );
        confirmed
    }

    fn verdict(&self, counterpart: Counterpart, confirmed: Vec<String>) -> Result<(), String> {
        if confirmed.is_empty() {
            return Ok(());
        }
        let consequence = match counterpart {
            Counterpart::Exchange => "本地「基线 + Fill」已不可信",
            Counterpart::Executor(_) => {
                "策略下单依据的持仓已与账本分叉（也可能是该 executor 或分发层积压数秒、\
                 期间恰有成交，先查管线滞后告警）"
            }
        };
        Err(format!(
            "持仓对账（本地 vs {counterpart}）连续 {} 次差值稳定的不一致，{consequence}: {}",
            self.max_consecutive,
            confirmed.join("; ")
        ))
    }
}

// ============================================================================
// 组件包装（含读数轮询）
// ============================================================================

/// [`PositionLedger::spawn`] 的参数
pub struct PositionLedgerArgs {
    /// 各所的账户私有能力（读数的来源）。键集 = 配了凭证的所 —— "没有账户的所不轮询"
    /// 由类型表达，本层不再过滤。
    pub accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 镜像的增量输入（只订实盘 Fill），也是持仓价值读数（[`PositionValues`]）发布的地方
    pub account_bus: AccountBus,
    /// 盘口（只订 BBO）：给持仓估值
    pub market_bus: MarketBus,
    /// 轮询间隔（毫秒）
    pub interval_ms: u64,
    /// 连续多少次不一致判定为真实漂移
    pub max_consecutive_mismatches: u32,
}

/// PositionLedger 的收件箱消息
pub enum PositionLedgerMsg {
    /// 账户总线上的实盘 Fill（订阅时已按 [`PositionBook::live_fill`] 过滤）
    Account(AccountEvent),
    /// 行情总线上的 BBO（只用来给持仓估值）
    Market(MarketEvent),
    /// 见 [`PositionLedgerHandle::register_symbols`]
    RegisterSymbols {
        symbols: Vec<Symbol>,
        baselines: Vec<PositionBaseline>,
        reply: oneshot::Sender<()>,
    },
    /// 见 [`PositionLedgerHandle::watch`]
    Watch(ExecutorHandle, oneshot::Sender<()>),
    /// 见 [`PositionLedgerHandle::unwatch`]
    Unwatch(ExecutorId, oneshot::Sender<()>),
}

/// PositionLedger 的地址（可克隆）。方法都是 ask：返回即已生效。
#[derive(Clone, Debug)]
pub struct PositionLedgerHandle(Addr<PositionLedgerMsg>);

impl PositionLedgerHandle {
    /// 投产注册握手：symbol 集合 + 持仓基线原子到达（与观测组件收到的是同一份）。
    pub async fn register_symbols(
        &self,
        symbols: Vec<Symbol>,
        baselines: Vec<PositionBaseline>,
    ) -> Result<(), Gone> {
        self.0
            .ask(|reply| PositionLedgerMsg::RegisterSymbols {
                symbols,
                baselines,
                reply,
            })
            .await
    }

    /// 把一个实盘 executor 加入监视名单（编排在它开始收事件之前调，见模块文档）。
    pub async fn watch(&self, executor: ExecutorHandle) -> Result<(), Gone> {
        self.0
            .ask(|reply| PositionLedgerMsg::Watch(executor, reply))
            .await
    }

    /// 把一个 executor 移出监视名单（编排在注销它的订阅**之前**调，见模块文档）。
    /// 不在名单里是空操作。
    pub async fn unwatch(&self, executor: ExecutorId) -> Result<(), Gone> {
        self.0
            .ask(|reply| PositionLedgerMsg::Unwatch(executor, reply))
            .await
    }
}

/// 持仓账本组件（账本与比对逻辑在 [`Reconciler`]，轮询与名单在 [`Ledger`]，本层只管收件与节拍）
pub struct PositionLedger {
    inbox: Inbox<PositionLedgerMsg>,
    interval: Duration,
    ledger: Ledger,
}

/// 一次拉取（或 executor 查询）的结果，在 `run` 里就地处理
enum Fetched {
    Exchange {
        exchange: Exchange,
        result: Result<Vec<Position>, String>,
    },
    Executor {
        executor: ExecutorId,
        result: Result<Vec<Position>, Gone>,
    },
}

/// 账本的全部状态与逻辑（不含收件箱，可脱离任务树单测）
struct Ledger {
    reconciler: Reconciler,
    accounts: HashMap<Exchange, Arc<dyn AccountClient>>,
    /// 停摆守卫（每所一个）：对账通道停摆等于风控失效，超过容忍窗口即致命。
    /// 判据用"距上次成功的时长"而非"连续失败次数"，改轮询间隔不会连带改变容忍窗口。
    guards: HashMap<Exchange, StalenessGuard>,
    /// 已发出、尚未返回结果的所（见 [`Self::start_fetches`]）
    in_flight: HashSet<Exchange>,
    /// 受监视的实盘 executor（名单由编排维护，见模块文档「两条比对」）
    executors: HashMap<ExecutorId, ExecutorHandle>,
    /// 已发出持仓查询、尚未返回的 executor。
    ///
    /// 不配停摆守卫：executor 迟迟不答多半是它收件箱积压，恢复后由它自己的管线滞后告警报出
    /// （`Executor::check_pipeline_lag`）；它停了则查询立即返回 `Gone`。永久卡死在某次处理里时
    /// 这条比对会静默失效 —— 不在本机制范围内，卡死的实例也下不了单。
    executor_in_flight: HashSet<ExecutorId>,
    /// 在途的拉取与查询，由 `run` 与收件箱一起 poll
    pending: FuturesUnordered<BoxFuture<'static, Fetched>>,
    /// 持仓价值读数的发布去处（见 [`Self::publish_values`]）
    account_bus: AccountBus,
    /// 各 (所, 币) 最新盘口中间价，给持仓估值
    mids: HashMap<(Exchange, Symbol), Price>,
    /// 整机停止状态：一进停机就不再发读数 —— 停机后不再下新单，读数没人用；而同在消费者组的
    /// 订阅者（分发层、指标）此时可能已关箱，再发会记 ERROR「事件丢失」（回灌边，见
    /// architecture.md V5）
    machine_stop: StopFlag,
}

impl PositionLedger {
    /// 在 `group`（consumers）下起账本，构造时订阅账户总线上的实盘 Fill，返回句柄与地址。
    pub async fn spawn(
        group: &Group,
        args: PositionLedgerArgs,
    ) -> anyhow::Result<(ChildHandle, PositionLedgerHandle)> {
        let (child, addr) = group
            .spawn_addressed(
                "PositionLedger",
                move |scope: Scope, mut inbox| async move {
                    // 镜像的增量输入只有实盘 Fill：订阅过滤与账本同一判据（模拟成交绝不进实盘镜像）
                    inbox.subscribe(
                        &args.account_bus,
                        |e| PositionBook::live_fill(e).is_some(),
                        PositionLedgerMsg::Account,
                    );
                    inbox.subscribe(
                        &args.market_bus,
                        |e| matches!(e.data, MarketData::BBO(_)),
                        PositionLedgerMsg::Market,
                    );

                    // 窗口从构造时刻起算，避免首次拉取前就被判成停摆
                    let guards = args
                        .accounts
                        .keys()
                        .map(|exchange| {
                            (
                                *exchange,
                                StalenessGuard::new(
                                    format!("{exchange} 持仓对账"),
                                    MAX_POLL_STALENESS_MS,
                                ),
                            )
                        })
                        .collect();

                    tracing::info!(
                        exchanges = args.accounts.len(),
                        interval_ms = args.interval_ms,
                        max_consecutive_mismatches = args.max_consecutive_mismatches,
                        "PositionLedger started"
                    );
                    Ok(Self {
                        inbox,
                        interval: Duration::from_millis(args.interval_ms),
                        ledger: Ledger {
                            reconciler: Reconciler::new(args.max_consecutive_mismatches),
                            accounts: args.accounts,
                            guards,
                            in_flight: HashSet::new(),
                            executors: HashMap::new(),
                            executor_in_flight: HashSet::new(),
                            pending: FuturesUnordered::new(),
                            account_bus: args.account_bus,
                            mids: HashMap::new(),
                            machine_stop: scope.machine_stop_flag(),
                        },
                    })
                },
            )
            .await?;
        Ok((child, PositionLedgerHandle(addr)))
    }
}

impl Task for PositionLedger {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 首拍立即；一次处理慢于周期时不积压补跑
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = self.inbox.recv() => match msg {
                    Some(msg) => self.ledger.handle(msg),
                    // 已被要求停止、箱已排空；在途的拉取随之丢弃（只读，丢了无害）
                    None => break,
                },
                _ = tick.tick() => self.ledger.on_tick().map_err(fatal)?,
                Some(done) = self.ledger.pending.next(), if !self.ledger.pending.is_empty() => {
                    self.ledger.on_fetched(done).map_err(fatal)?;
                }
            }
        }
        tracing::info!("PositionLedger stopped");
        Ok(())
    }
}

/// 本地持仓已不可信，受控退出（`run` 返回这个错误，整机停机）。
///
/// 策略的三道风控闸门（单边杠杆 / 账户杠杆 / 仓位上限）全部基于它计算，继续跑等于
/// 无风控裸奔。退出后交人工核对，容器编排负责拉起。
fn fatal(reason: String) -> anyhow::Error {
    anyhow::anyhow!(
        "持仓对账确认漂移或通道停摆，退出（本地持仓不可信，风控闸门已失效，需人工核对后重启）: {reason}"
    )
}

impl Ledger {
    fn handle(&mut self, msg: PositionLedgerMsg) {
        // 回执发送失败 = 提问方已不再等，没有人需要这份答复
        match msg {
            // 账本只吃实盘 Fill（订阅时已预过滤，on_event 内按账户再过滤）；
            // 持仓一变就把该所的价值发出去，别的币的策略实例不用等节拍就能看到
            PositionLedgerMsg::Account(event) => {
                self.reconciler.on_event(&event);
                if let AccountData::Symbol(d) = &event.data {
                    self.publish_values(d.exchange());
                }
            }
            PositionLedgerMsg::Market(MarketEvent {
                data: MarketData::BBO(bbo),
                ..
            }) => {
                let mid = bbo.mid_price();
                match self.mids.get_mut(&(bbo.exchange, bbo.symbol.clone())) {
                    Some(m) => *m = mid,
                    None => {
                        self.mids.insert((bbo.exchange, bbo.symbol.clone()), mid);
                    }
                }
            }
            PositionLedgerMsg::Market(_) => {}
            PositionLedgerMsg::RegisterSymbols {
                symbols,
                baselines,
                reply,
            } => {
                tracing::info!(count = symbols.len(), "PositionLedger tracking symbols");
                self.reconciler.register(&symbols, &baselines);
                let _ = reply.send(());
            }
            PositionLedgerMsg::Watch(executor, reply) => {
                tracing::info!(executor = %executor.id(), "开始监视实盘 executor 的持仓");
                self.executors.insert(executor.id(), executor);
                let _ = reply.send(());
            }
            PositionLedgerMsg::Unwatch(executor, reply) => {
                // 它的不一致记录不必清：ExecutorId 不复用，残留记录不会再被读到
                self.executors.remove(&executor);
                let _ = reply.send(());
            }
        }
    }

    /// 轮询节拍：发起各所拉取与 executor 查询（结果稍后在 `run` 里到达）。`Err` = 应致命退出。
    fn on_tick(&mut self) -> Result<(), String> {
        // 没有任何配了凭证的所时无事可做（引擎只接公共行情）
        if self.accounts.is_empty() {
            return Ok(());
        }
        // 先判在途停摆：请求挂死时，守卫的两个触点都够不到，只有这里能发现
        self.check_in_flight_staleness()?;
        self.start_fetches();
        self.start_executor_queries();
        // 没有成交时价格也在变：每拍按最新盘口重发一遍
        let exchanges: Vec<Exchange> = self.accounts.keys().copied().collect();
        for exchange in exchanges {
            self.publish_values(exchange);
        }
        Ok(())
    }

    /// 把该所各币的持仓价值发到账户总线（按所投给订了该所的策略）。有仓的币还没有盘口时
    /// 不发：读数留在上一份（启动时就是「未知」，策略不开仓）。
    fn publish_values(&self, exchange: Exchange) {
        if self.machine_stop.is_stopping() {
            return;
        }
        let mid_of = |symbol: &Symbol| self.mids.get(&(exchange, symbol.clone())).copied();
        match self.reconciler.position_values(exchange, mid_of) {
            Ok(legs) => {
                let ts = now_ms();
                self.account_bus.publish(AccountEvent {
                    account: AccountId::Live,
                    exchange_ts: ts,
                    local_ts: ts,
                    data: AccountData::Exchange(ExchangeAccountData::PositionValues(
                        PositionValues { exchange, legs },
                    )),
                });
            }
            Err(symbol) => {
                tracing::warn!(%exchange, %symbol, "该币有仓但还没有盘口，暂不发该所持仓价值（策略沿用上一份读数，启动时为未知、不开仓）");
            }
        }
    }

    /// 一次拉取或查询的结果：比对；`Err` = 确认漂移、分叉或通道停摆，应致命退出。
    fn on_fetched(&mut self, done: Fetched) -> Result<(), String> {
        match done {
            Fetched::Exchange { exchange, result } => {
                self.in_flight.remove(&exchange);
                self.ingest(exchange, result)
            }
            Fetched::Executor { executor, result } => self.ingest_executor(executor, result),
        }
    }

    /// 在途未回的所是否已停摆过久。`Err` = 应致命退出。
    ///
    /// # 为什么这条判据必须存在
    ///
    /// [`StalenessGuard`] 的两个触点（`record_success` / `check_failure`）都在**结果到达**的
    /// 路径上。而"迟迟不返回"是可达的：client 的超时失效导致请求真挂死。那时 `in_flight`
    /// 永久占位，此后每个节拍都只是跳过 —— **该所对账静默永久失效，而引擎继续无校验地交易**。
    ///
    /// 所以"在途未回"必须与"拉取失败"同等对待：都是没拿到读数，都要过守卫。判据仍是
    /// "距上次成功的时长"，与在途了几轮无关。
    ///
    /// （拉取 panic 不在此列：拉取在本任务里被 poll，panic 随 `run` 上冒。）
    fn check_in_flight_staleness(&self) -> Result<(), String> {
        for exchange in &self.in_flight {
            let guard = self.guards.get(exchange).expect("guard 与 client 同建");
            guard.check_failure(format!("{exchange} 上一轮持仓读数始终未返回"))?;
            tracing::warn!(
                %exchange,
                stale_ms = guard.stale_ms(),
                "上一轮持仓读数尚未返回，本轮跳过（REST 慢于轮询间隔）"
            );
        }
        Ok(())
    }

    /// 为每个**当前没有在途请求**的所发起一次拉取，结果稍后在 `run` 里到达。
    ///
    /// 跳过在途的所（而非并发再发一次）：REST 慢于轮询间隔时，堆积并发请求既压垮限流额度，
    /// 又让"最新读数"的时点无从判断。跳过是否已到停摆程度由
    /// [`Self::check_in_flight_staleness`] 判定，节拍先过那道再进本函数。
    fn start_fetches(&mut self) {
        for (exchange, client) in &self.accounts {
            if !self.in_flight.insert(*exchange) {
                continue;
            }
            let (exchange, client) = (*exchange, client.clone());
            self.pending.push(Box::pin(async move {
                let result = client.fetch_positions().await.map_err(|e| e.to_string());
                Fetched::Exchange { exchange, result }
            }));
        }
    }

    /// 向每个受监视、且没有在途查询的 executor 要一份持仓。与拉取一样放进在途集合：
    /// executor 收件箱积压时，本组件不该跟着卡住（快照查询与 Fill 摄入都在本组件的收件箱里）。
    fn start_executor_queries(&mut self) {
        for (id, executor) in &self.executors {
            if !self.executor_in_flight.insert(*id) {
                continue;
            }
            let (id, executor) = (*id, executor.clone());
            self.pending.push(Box::pin(async move {
                let result = executor.seeded_positions().await;
                Fetched::Executor {
                    executor: id,
                    result,
                }
            }));
        }
    }

    /// 处理一个 executor 的持仓答复。`Err` = 已确认分叉，调用方应致命退出。
    fn ingest_executor(
        &mut self,
        executor: ExecutorId,
        result: Result<Vec<Position>, Gone>,
    ) -> Result<(), String> {
        self.executor_in_flight.remove(&executor);
        // 查询在途期间被撤出名单：它已不再收 Fill，这份答复不该拿来比
        if !self.executors.contains_key(&executor) {
            return Ok(());
        }
        match result {
            Ok(positions) => self.reconciler.reconcile_executor(executor, &positions),
            Err(Gone) => {
                // executor 已停却没有先撤出名单：账本是一对多分发方，摘掉它、免得每个节拍
                // 重复报同一件事（来路见 docs/architecture.md 3.1「Gone 的来路与处理」）
                tracing::warn!(%executor, "executor 持仓查询失败（已停止收件），移出监视名单");
                self.executors.remove(&executor);
                Ok(())
            }
        }
    }

    /// 处理一所的读数：更新守卫、比对。`Err` = 确认漂移或通道停摆过久，调用方应致命退出。
    fn ingest(
        &mut self,
        exchange: Exchange,
        result: Result<Vec<Position>, String>,
    ) -> Result<(), String> {
        let guard = self
            .guards
            .get_mut(&exchange)
            .expect("guard 与 client 同建");
        match result {
            Ok(positions) => {
                guard.record_success();
                tracing::debug!(%exchange, count = positions.len(), "Position report polled");
                self.reconciler.reconcile(exchange, &positions)
            }
            Err(e) => {
                // REST 抖动（超时、限流、5xx）是常态，单次失败只 warn；
                // 长期停摆由 guard 判定致命
                guard.check_failure(&e)?;
                tracing::warn!(
                    %exchange,
                    error = %e,
                    stale_ms = guard.stale_ms(),
                    "Failed to fetch positions for reconciliation, will retry"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fill, FillReason, Side};
    use crate::messaging::{AccountData, SymbolAccountData};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    const EX: Exchange = Exchange::Binance;
    const OTHER_EX: Exchange = Exchange::OKX;
    const SYM: &str = "BTC";
    /// 测试基线统一的快照请求时刻；需要累加的 Fill 应带更晚的 local_ts
    const SNAPSHOT_TS: u64 = 1;

    /// 注册 + seed 基线（投产握手），SYM 一个 symbol
    fn reconciler_with(max_consecutive: u32, baselines: &[PositionBaseline]) -> Reconciler {
        let mut r = Reconciler::new(max_consecutive);
        r.register(&[SYM.to_string()], baselines);
        r
    }

    fn position(exchange: Exchange, size: Decimal) -> Position {
        Position {
            exchange,
            symbol: SYM.to_string(),
            size,
        }
    }

    fn baseline_on(exchange: Exchange, size: Decimal) -> PositionBaseline {
        PositionBaseline {
            position: position(exchange, size),
            snapshot_req_ts: SNAPSHOT_TS,
        }
    }

    fn fill_on(exchange: Exchange, side: Side, size: Decimal, local_ts: u64) -> AccountEvent {
        AccountEvent {
            account: crate::domain::AccountId::Live,
            exchange_ts: local_ts,
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::Fill(Fill {
                exchange,
                symbol: SYM.to_string(),
                side,
                price: dec!(100.0),
                size,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: local_ts,
                fee: 0.0,
                reason: FillReason::Normal,
            })),
        }
    }

    fn fill(side: Side, size: Decimal) -> AccountEvent {
        fill_on(EX, side, size, SNAPSHOT_TS + 1)
    }

    #[test]
    fn matching_position_is_not_drift() {
        let mut r = reconciler_with(3, &[baseline_on(EX, dec!(2.0))]);
        r.on_event(&fill(Side::Long, dec!(1.0)));
        // 本地 3.0，交易所也 3.0
        for _ in 0..10 {
            r.reconcile(EX, &[position(EX, dec!(3.0))]).unwrap();
        }
    }

    /// 数量精确，没有容差：差一个最小步长也是漂移（f64 时曾按步长留容差吸收累加噪声）
    #[test]
    fn any_nonzero_diff_is_drift() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(1.0))]);
        r.reconcile(EX, &[position(EX, dec!(1.001))])
            .expect_err("差 0.001 也是漂移");
    }

    /// 成交累加精确归零：0.1 + 0.2 - 0.3 与交易所的 0 一致（f64 下差 5.5e-17，曾靠容差兜住）
    #[test]
    fn accumulated_fills_match_exactly() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(0.1))]);
        r.on_event(&fill(Side::Long, dec!(0.2)));
        r.on_event(&fill(Side::Short, dec!(0.3)));
        r.reconcile(EX, &[]).expect("本地 0 与交易所空仓一致");
    }

    /// 连续 N 次不一致才致命；第 N 次才返回 Err
    #[test]
    fn drift_is_fatal_only_after_n_consecutive_mismatches() {
        let mut r = reconciler_with(3, &[baseline_on(EX, dec!(1.0))]);

        r.reconcile(EX, &[position(EX, dec!(5.0))]).unwrap();
        r.reconcile(EX, &[position(EX, dec!(5.0))]).unwrap();
        let err = r
            .reconcile(EX, &[position(EX, dec!(5.0))])
            .expect_err("连续第 3 次不一致应致命");
        assert!(err.contains("连续 3 次差值稳定的不一致"), "got: {err}");
    }

    /// **误停机防线**：差值每次都在变 = 在途成交造成的错位，不是固定漂移，不该累加。
    ///
    /// 某 symbol 持续成交时，每次轮询都可能正好撞在"成交已落交易所、Fill 尚未送达"的窗口
    /// 上。只数次数会把它攒成致命 —— 而误停机比漏报更糟。
    #[test]
    fn changing_diff_never_accumulates_into_a_fatal() {
        let mut r = reconciler_with(3, &[baseline_on(EX, dec!(1.0))]);

        // 本地始终 1.0，交易所读数每次不同 -> 差值每次不同 -> 永远停在 streak=1
        for reported in [
            dec!(3.0),
            dec!(7.0),
            dec!(2.5),
            dec!(9.0),
            dec!(4.25),
            dec!(6.5),
            dec!(8.75),
        ] {
            r.reconcile(EX, &[position(EX, reported)])
                .expect("差值在变说明是在途成交，不该判为漂移");
        }
    }

    /// 反面：真漂移的差值**恒定**，即使本地持仓一直在变也能检出。
    ///
    /// 漏算一笔 X 之后，本地与交易所会同步移动，差值仍是 X —— 这正是"差值稳定"判据既不
    /// 误伤飞行窗口、又不会在活跃交易期压制真实漂移的原因。
    #[test]
    fn constant_diff_is_detected_even_while_position_keeps_moving() {
        let mut r = reconciler_with(3, &[baseline_on(EX, dec!(10.0))]);

        // 交易所比本地少 2（漏算了一笔 -2 的成交），此后持续成交、两边同步移动
        r.reconcile(EX, &[position(EX, dec!(8.0))]).unwrap();
        r.on_event(&fill(Side::Long, dec!(1.0))); // 本地 11
        r.reconcile(EX, &[position(EX, dec!(9.0))]).unwrap(); // 差值仍 +2
        r.on_event(&fill(Side::Long, dec!(3.0))); // 本地 14
        let err = r
            .reconcile(EX, &[position(EX, dec!(12.0))]) // 差值仍 +2
            .expect_err("差值恒定即真漂移，持续交易不该把它抹掉");
        assert!(err.contains("本地 14"), "got: {err}");
    }

    /// 差值反向也算变化：+X 与 -X 是方向相反的两种漂移，不能接续计数
    #[test]
    fn sign_flip_in_diff_restarts_the_streak() {
        let mut r = reconciler_with(2, &[baseline_on(EX, dec!(5.0))]);

        // 差值 -3（交易所多）
        r.reconcile(EX, &[position(EX, dec!(8.0))]).unwrap();
        // 差值 +3（本地多）—— 绝对值相同但方向相反，必须重新从 1 数起
        r.reconcile(EX, &[position(EX, dec!(2.0))])
            .expect("差值反向不该接着上一次的计数");
        // 再来一次 +3 才够 2 次
        let err = r
            .reconcile(EX, &[position(EX, dec!(2.0))])
            .expect_err("同方向连续两次应致命");
        assert!(err.contains("交易所 2"), "got: {err}");
    }

    /// **关键**：中间只要一致一次就清零，计数器衡量的是"持续不一致"而非历史累计。
    /// 否则飞行窗口（成交已落交易所、Fill 稍后才到）攒够次数就会误停机。
    #[test]
    fn a_single_match_resets_the_streak() {
        let mut r = reconciler_with(3, &[baseline_on(EX, dec!(1.0))]);

        r.reconcile(EX, &[position(EX, dec!(5.0))]).unwrap();
        r.reconcile(EX, &[position(EX, dec!(5.0))]).unwrap();
        // 一致一次 -> 清零
        r.reconcile(EX, &[position(EX, dec!(1.0))]).unwrap();
        // 再来两次不一致仍不该致命（重新从 1 数起）
        r.reconcile(EX, &[position(EX, dec!(5.0))]).unwrap();
        r.reconcile(EX, &[position(EX, dec!(5.0))])
            .expect("清零后只累计到 2，不应致命");
    }

    /// **最危险的一类漂移**：本地有仓、交易所读数里根本没有这个 symbol。
    ///
    /// 读数是完整快照，"没报告"就等于空仓；若按"没报告就跳过"处理，漏掉的正好是
    /// 平仓 Fill 丢失 / 强平回报丢失这两种最该抓的情况。
    #[test]
    fn local_position_absent_from_report_is_drift() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(2.0))]);

        let err = r
            .reconcile(EX, &[])
            .expect_err("本地有仓而读数为空必须判为漂移");
        assert!(err.contains("交易所 0"), "got: {err}");
    }

    /// 反向：本地空仓、交易所却有仓（漏了一笔开仓 Fill）。
    ///
    /// 基线本身是 0（Manager 对交易所未返回的 symbol 会显式给 size=0 的基线），
    /// 此后交易所冒出仓位就说明漏了成交。
    #[test]
    fn unknown_exchange_position_is_drift() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(0.0))]);
        let err = r
            .reconcile(EX, &[position(EX, dec!(1.5))])
            .expect_err("本地空仓而交易所有仓必须判为漂移");
        assert!(err.contains("本地 0"), "got: {err}");
    }

    /// **基线未到之前不对账**：那时本地持仓是"未知"，不是 0。
    ///
    /// 某腿的所未配置 client 时没有基线可 seed，该腿不参与对账 ——
    /// 交易所报了个大仓位也不该判漂移。
    #[test]
    fn no_reconciliation_for_unseeded_legs() {
        // 注册了 symbol，但没有任何基线（如该所未配置 client）
        let mut r = reconciler_with(1, &[]);
        for _ in 0..5 {
            r.reconcile(EX, &[position(EX, dec!(123.0))])
                .expect("基线未写入的腿不应对账");
        }
        // 基线写入后才开始比对
        r.register(&[SYM.to_string()], &[baseline_on(EX, dec!(123.0))]);
        r.reconcile(EX, &[position(EX, dec!(123.0))])
            .expect("基线与读数一致");
        let err = r
            .reconcile(EX, &[position(EX, dec!(0.0))])
            .expect_err("基线之后出现真实分歧才该报");
        assert!(err.contains("本地 123"), "got: {err}");
    }

    /// **防双计**：快照请求时刻之前送达的 Fill 已含在快照里，seed 后到达必须被过滤。
    ///
    /// 规则住在 `SymbolPositions::apply_fill`，账本与策略门面调的是**同一个函数** ——
    /// 不是"两处各写一遍、约定保持一致"。
    #[test]
    fn fills_covered_by_snapshot_are_not_double_counted() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(2.0))]);
        // local_ts == SNAPSHOT_TS：成交已含在快照里，丢弃
        r.on_event(&fill_on(EX, Side::Long, dec!(1.0), SNAPSHOT_TS));
        // local_ts > SNAPSHOT_TS：快照之后的新成交，累加
        r.on_event(&fill_on(EX, Side::Long, dec!(4.0), SNAPSHOT_TS + 5));
        // 镜像 = 2.0(快照) + 4.0 = 6.0
        r.reconcile(EX, &[position(EX, dec!(6.0))])
            .expect("快照已含的 Fill 不该重复累加");
        let err = r
            .reconcile(EX, &[position(EX, dec!(7.0))])
            .expect_err("若旧 Fill 也被累加，本地会是 7.0 而这里不会报漂移");
        assert!(err.contains("本地 6"), "got: {err}");
    }

    /// **注册前到达的 Fill 被丢弃，且不污染其后的 seed**（删缓冲重放后的核心行为）。
    ///
    /// 未注册 symbol 的 Fill 属于别的桶或尚未投产的 symbol，镜像无从归属只能丢弃；
    /// 随后注册携带的快照（已含该成交，若它属于本账户）是权威值，从快照起算即正确。
    #[test]
    fn fill_before_registration_is_dropped_and_does_not_pollute_seed() {
        let mut r = Reconciler::new(1);
        // 尚未注册任何 symbol：Fill 到达被丢弃
        r.on_event(&fill_on(EX, Side::Long, dec!(5.0), SNAPSHOT_TS + 1));
        // 随后注册并 seed（快照读到 2.0）—— 之前丢弃的 Fill 不得出现在镜像里
        r.register(&[SYM.to_string()], &[baseline_on(EX, dec!(2.0))]);
        r.reconcile(EX, &[position(EX, dec!(2.0))])
            .expect("镜像应恰为快照值 2.0，注册前的 Fill 不得混入");
        let err = r
            .reconcile(EX, &[position(EX, dec!(7.0))])
            .expect_err("若被丢弃的 Fill 混入了镜像，本地会是 7.0 而这里不会报漂移");
        assert!(err.contains("本地 2"), "got: {err}");
    }

    /// 重复注册（同一 symbol 再次投产）不得覆写镜像存量：镜像一直在跟 Fill
    #[test]
    fn re_registration_never_overwrites_the_mirror() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(2.0))]);
        r.on_event(&fill(Side::Long, dec!(1.0))); // 镜像 3.0
                                                  // 再次投产：新快照读到 3.0，但镜像已 seed 过 —— 静默跳过，不覆写
        r.register(
            &[SYM.to_string()],
            &[PositionBaseline {
                position: position(EX, dec!(3.0)),
                snapshot_req_ts: 100,
            }],
        );
        r.reconcile(EX, &[position(EX, dec!(3.0))])
            .expect("镜像应保持 3.0（基线 2.0 + Fill 1.0），与交易所一致");
    }

    /// 跟踪范围外的 symbol 不参与对账（分桶部署下账户级读数含其他桶的 symbol）
    #[test]
    fn untracked_symbols_are_ignored() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(1.0))]);
        let other_bucket = Position {
            exchange: EX,
            symbol: "ETH".to_string(),
            size: dec!(42.0),
        };
        // 读数里混入其他桶的 symbol；本桶的 BTC 对得上，就不该有漂移
        r.reconcile(EX, &[position(EX, dec!(1.0)), other_bucket])
            .expect("其他桶的 symbol 不该触发漂移");
    }

    /// 计数按 (所, symbol) 分开：一个所的漂移不该借另一个所的次数达标
    #[test]
    fn streaks_are_per_exchange() {
        let mut r = reconciler_with(
            2,
            &[baseline_on(EX, dec!(1.0)), baseline_on(OTHER_EX, dec!(0.0))],
        );

        // Binance 不一致一次
        r.reconcile(EX, &[position(EX, dec!(9.0))]).unwrap();
        // OKX 上本地与读数都是 0 -> 一致，不该影响 Binance 的计数
        r.reconcile(OTHER_EX, &[]).unwrap();
        // Binance 第二次不一致 -> 达标
        let err = r
            .reconcile(EX, &[position(EX, dec!(9.0))])
            .expect_err("同一个所连续两次应致命");
        assert!(err.contains("Binance"), "got: {err}");
    }

    /// 同一 symbol 跨所独立：Binance 漂了，OKX 那条腿的读数不受影响
    #[test]
    fn per_exchange_legs_are_compared_independently() {
        let mut r = reconciler_with(
            1,
            &[
                baseline_on(EX, dec!(1.0)),
                baseline_on(OTHER_EX, dec!(-1.0)),
            ],
        );

        // OKX 腿一致
        r.reconcile(OTHER_EX, &[position(OTHER_EX, dec!(-1.0))])
            .expect("OKX 腿对得上");
        // Binance 腿不一致
        let err = r
            .reconcile(EX, &[position(EX, dec!(4.0))])
            .expect_err("Binance 腿应报漂移");
        assert!(err.contains("Binance"), "got: {err}");
        assert!(!err.contains("OKX"), "不该牵连对得上的 OKX 腿: {err}");
    }

    // ---------------- executor 持仓纳入对账（#694） ----------------

    fn executor() -> ExecutorId {
        ExecutorId::for_test(u64::MAX - 7)
    }

    /// executor 与账本一致：任意多轮都不报
    #[test]
    fn executor_matching_the_ledger_is_not_drift() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(2.0))]);
        r.on_event(&fill(Side::Long, dec!(1.0)));
        for _ in 0..5 {
            r.reconcile_executor(executor(), &[position(EX, dec!(3.0))])
                .unwrap();
        }
    }

    /// **本 issue 要抓的情形**：账本与交易所对得上，只有 executor（下单依据）错了。
    /// 交易所那条比对永远看不见它，必须由 executor 这条在第 N 次稳定差值时报出。
    #[test]
    fn executor_diverging_from_a_correct_ledger_is_fatal_after_n_rounds() {
        let mut r = reconciler_with(3, &[baseline_on(EX, dec!(2.0))]);
        for round in 1..=3 {
            r.reconcile(EX, &[position(EX, dec!(2.0))])
                .expect("账本与交易所一致，交易所那条比对无从发现");
            let result = r.reconcile_executor(executor(), &[position(EX, dec!(5.0))]);
            if round < 3 {
                result.expect("未满 N 次不致命（可能只是在途）");
            } else {
                let err = result.expect_err("连续 N 次稳定差值必须致命");
                assert!(err.contains("executor"), "报错要指明是哪个对手方: {err}");
            }
        }
    }

    /// 两种对手方各自计数：交易所那边的不一致不能替 executor 凑次数，反之亦然
    #[test]
    fn executor_and_exchange_streaks_are_independent() {
        let mut r = reconciler_with(2, &[baseline_on(EX, dec!(2.0))]);
        r.reconcile(EX, &[position(EX, dec!(9.0))]).unwrap();
        r.reconcile_executor(executor(), &[position(EX, dec!(9.0))])
            .expect("executor 只不一致了一次，不能借交易所那边的次数达标");
    }

    /// 账本那边未 seed 的腿（该所无凭证）executor 记为 0 —— 不可比，跳过
    #[test]
    fn executor_legs_unseeded_in_the_ledger_are_skipped() {
        let mut r = reconciler_with(1, &[baseline_on(EX, dec!(2.0))]);
        r.reconcile_executor(
            executor(),
            &[position(EX, dec!(2.0)), position(OTHER_EX, dec!(0.0))],
        )
        .expect("OTHER_EX 在账本里是未知，不该参与比对");
        // 即便账本靠 Fill 在那条腿上累加出了数，也是未知起点，不比
        r.on_event(&fill_on(OTHER_EX, Side::Long, dec!(1.0), SNAPSHOT_TS + 1));
        r.reconcile_executor(
            executor(),
            &[position(EX, dec!(2.0)), position(OTHER_EX, dec!(0.0))],
        )
        .expect("未 seed 的腿不比");
    }

    /// 对外读数只含**基线已写入**的腿，且空仓腿必须如实报出 0。
    ///
    /// 两个方向都要守：漏报空仓腿会让下游把"已平仓"看成"未知"；把未 seed 的腿当 0 报出去
    /// 则相反 —— 下游会据此算出假的对冲完整性。
    #[test]
    fn readings_contain_seeded_legs_only_including_flat_ones() {
        let mut r = reconciler_with(
            1,
            &[baseline_on(EX, dec!(2.0)), baseline_on(OTHER_EX, dec!(0.0))],
        );
        r.on_event(&fill(Side::Long, dec!(1.0))); // EX 腿 -> 3.0
        let size_on = |ex| {
            r.position_values(ex, |_: &Symbol| Some(dec!(1)))
                .unwrap()
                .get(SYM)
                .map(LegValue::size)
        };
        assert_eq!(size_on(EX), Some(dec!(3.0)));
        assert_eq!(
            size_on(OTHER_EX),
            Some(dec!(0.0)),
            "空仓腿也必须报出，否则下游分不清「已平仓」与「未知」"
        );
    }

    /// 未 seed 的腿绝不出现在读数里：那是"未知"，不是"空仓"。
    #[test]
    fn readings_omit_unseeded_legs() {
        // 注册了 symbol 但没有任何基线（该所未配置凭证）
        let mut r = reconciler_with(1, &[]);
        // 即便 Fill 已经累加出仓位，未 seed 也不外报 —— 起点未知，这个数没有意义
        r.on_event(&fill(Side::Long, dec!(1.0)));
        let readings = r.position_values(EX, |_: &Symbol| Some(dec!(1))).unwrap();
        assert!(
            readings.is_empty(),
            "未 seed 的腿不得进入读数: {readings:?}"
        );
    }

    /// #759：读数带持仓与中间价，价值 = |持仓| × 中间价（空头也为正）；空仓的币不需要价；未 seed 的腿不报
    #[test]
    fn position_values_are_abs_size_times_mid_and_flat_legs_need_no_price() {
        let mut r = Reconciler::new(1);
        r.register(
            &[SYM.to_string(), "ETH".to_string()],
            &[
                baseline_on(EX, dec!(-2.0)),
                baseline_on(OTHER_EX, dec!(0.0)),
            ],
        );
        r.on_event(&fill_on(EX, Side::Short, dec!(1.0), SNAPSHOT_TS + 1)); // EX: -3
        let mid = |s: &Symbol| (s == SYM).then_some(dec!(100));
        let held = r.position_values(EX, mid).unwrap();
        assert_eq!(held.len(), 1, "ETH 未 seed，不报: {held:?}");
        assert_eq!(held[SYM].size(), dec!(-3));
        assert_eq!(held[SYM].value(), 300.0);
        let flat = r.position_values(OTHER_EX, |_: &Symbol| None).unwrap();
        assert_eq!(flat[SYM].value(), 0.0, "空仓不需要价");
    }

    /// 有仓却没价：价值未知，不能当 0 报出去（杠杆闸门会少算）
    #[test]
    fn a_held_symbol_without_a_price_makes_the_values_unknown() {
        let r = reconciler_with(1, &[baseline_on(EX, dec!(1.0))]);
        assert_eq!(
            r.position_values(EX, |_: &Symbol| None),
            Err(SYM.to_string())
        );
    }

    /// 镜像只吃基线与成交：对账读数本身绝不能写进镜像（否则漂移会被自我抹平）
    #[test]
    fn report_never_feeds_the_mirror() {
        let mut r = reconciler_with(2, &[baseline_on(EX, dec!(1.0))]);
        // 第一次不一致
        r.reconcile(EX, &[position(EX, dec!(7.0))]).unwrap();
        // 若读数写进了镜像，这次就会"一致"从而清零，永远等不到致命
        let err = r
            .reconcile(EX, &[position(EX, dec!(7.0))])
            .expect_err("读数若写进镜像，漂移会被自我抹平");
        assert!(err.contains("本地 1"), "镜像被读数污染了: {err}");
    }
}

/// 硬要求（用户 11-12）：多 dex 下「启动 REST 基线 + Fill 流」与定时 REST 对账照样成立。
/// 用 HL 适配层真实的解析路径（持仓快照合并、私有推送过滤）喂 [`Reconciler`]。
#[cfg(test)]
mod hl_multi_dex_tests {
    use super::*;
    use crate::exchange::hyperliquid::testkit::{
        fills_from_ws, markets, positions, state_json, user_fills_json,
    };

    const EX: Exchange = Exchange::Hyperliquid;
    const SNAPSHOT_TS: u64 = 10;
    const N: u32 = 3;

    /// 主站 + xyz 两个 dex；基线：BTC 多 1、TSLA 空 2
    fn setup() -> (crate::exchange::hyperliquid::markets::HlMarkets, Reconciler) {
        let m = markets(&["", "xyz"]);
        let base = positions(
            &m,
            &[
                ("", &state_json(&[("BTC", 1.0)])),
                ("xyz", &state_json(&[("xyz:TSLA", -2.0)])),
            ],
        );
        assert_eq!(base.len(), 2, "基线必须含两个 dex 的仓位");
        let baselines: Vec<_> = base
            .into_iter()
            .map(|position| PositionBaseline {
                position,
                snapshot_req_ts: SNAPSHOT_TS,
            })
            .collect();
        let mut r = Reconciler::new(N);
        r.register(&["BTC".to_string(), "TSLA".to_string()], &baselines);
        (m, r)
    }

    /// 交易所侧之后的真实持仓：BTC 1.5、TSLA -3（各 dex 一份快照）
    fn rest_snapshot(m: &crate::exchange::hyperliquid::markets::HlMarkets) -> Vec<Position> {
        positions(
            m,
            &[
                ("", &state_json(&[("BTC", 1.5)])),
                ("xyz", &state_json(&[("xyz:TSLA", -3.0)])),
            ],
        )
    }

    #[test]
    fn baseline_plus_fills_matches_rest_across_dexes() {
        let (m, mut r) = setup();
        // 一条推送里混着两个 dex 的成交：主站 BTC 买 0.5、xyz TSLA 卖 1
        let raw = user_fills_json(&[("BTC", "B", 0.5), ("xyz:TSLA", "A", 1.0)]);
        let events = fills_from_ws(&m, &raw, SNAPSHOT_TS + 1).unwrap();
        assert_eq!(events.len(), 2, "两个 dex 的成交都要进 Fill 流");
        // 未接入的 flx 上同名 TSLA 的成交：不进 Fill 流（直接丢弃），不会串到 xyz:TSLA
        let foreign = user_fills_json(&[("flx:TSLA", "B", 5.0)]);
        let foreign = fills_from_ws(&m, &foreign, SNAPSHOT_TS + 1).unwrap();
        assert!(foreign.is_empty(), "目录外成交不产生任何事件");
        for e in &events {
            r.on_event(e);
        }
        let snapshot = rest_snapshot(&m);
        for _ in 0..10 {
            r.reconcile(EX, &snapshot)
                .expect("基线 + Fill 与 REST 一致，不应报漂移");
        }
    }

    #[test]
    fn a_missed_fill_on_one_dex_becomes_drift_after_n_rounds() {
        let (m, mut r) = setup();
        // xyz 的那笔成交丢了，只收到主站的
        let raw = user_fills_json(&[("BTC", "B", 0.5)]);
        for e in &fills_from_ws(&m, &raw, SNAPSHOT_TS + 1).unwrap() {
            r.on_event(e);
        }
        let snapshot = rest_snapshot(&m);
        for round in 1..N {
            r.reconcile(EX, &snapshot)
                .unwrap_or_else(|e| panic!("第 {round} 次就判漂移: {e}"));
        }
        let err = r
            .reconcile(EX, &snapshot)
            .expect_err("连续 N 次稳定差值应判漂移");
        assert!(err.contains("TSLA"), "{err}");
        assert!(!err.contains("BTC"), "主站那条腿是一致的: {err}");
    }
}

/// 组件层：收件箱在任何时刻都不阻塞在 REST 上（快照面契约的硬性要求）、在途判据、
/// executor 持仓纳入对账（#694）。用 `tasks::Root` 搭树测。
#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::domain::{AccountInfo, Decimal, ExchangeError, OrderId, PriceStep, SymbolMeta};
    use crate::engine::live::executor::{Executor, ExecutorArgs};
    use crate::engine::live::OutcomeBus;
    use crate::exchange::ExchangeOrder;
    use crate::tasks::Root;
    use rust_decimal_macros::dec;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";
    /// 大到测试期间不会自行触发第二次节拍（首拍在 run 开始时立即触发）
    const NEVER_AGAIN_MS: u64 = 3_600_000;
    /// 需要多轮节拍的用例用的短周期
    const FAST_MS: u64 = 10;

    /// `fetch_positions` 的故障形态。共用一个 client，避免把 7 个
    /// `unreachable!` 的方法抄几遍。
    enum Faulty {
        /// 永不返回。不用 `sleep` 制造卡死：普通运行时里 sleep 会真的等，而 paused 运行时
        /// 空闲时会自动推进时钟把 sleep 走完 —— 两种都测不到"卡死"。信号量不涉及时钟。
        Hang,
        /// 拉取过程中 panic（adapter 里的 unwrap 之类）
        Panic,
        /// 不是故障：每次都如实返回这个持仓（给只关心 executor 那条比对的用例）
        Steady(Decimal),
    }

    struct FaultyClient {
        mode: Faulty,
        gate: Semaphore,
        calls: Arc<AtomicUsize>,
    }

    impl FaultyClient {
        /// 返回 (client, 调用计数) 两个句柄 —— 计数由测试持有，用于断言发起了几次拉取
        fn pair(mode: Faulty) -> (Arc<dyn AccountClient>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let client = Arc::new(Self {
                mode,
                gate: Semaphore::new(0),
                calls: calls.clone(),
            });
            (client, calls)
        }
    }

    #[async_trait::async_trait]
    impl AccountClient for FaultyClient {
        fn exchange(&self) -> Exchange {
            EX
        }
        async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                Faulty::Panic => panic!("模拟 adapter 内的 panic"),
                Faulty::Steady(size) => Ok(vec![Position {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    size,
                }]),
                Faulty::Hang => {
                    let _permit = self.gate.acquire().await;
                    unreachable!("信号量永不放行")
                }
            }
        }
        async fn place_order(
            &self,
            _order: ExchangeOrder,
        ) -> Result<OrderId, crate::domain::PlaceError> {
            unreachable!("账本不下单")
        }
        async fn cancel_order(
            &self,
            _symbol: &Symbol,
            _client_order_id: &str,
        ) -> Result<(), ExchangeError> {
            unreachable!("账本不撤单")
        }
        async fn fetch_open_orders(
            &self,
        ) -> Result<Vec<crate::exchange::OpenOrder>, ExchangeError> {
            unreachable!("账本不查挂单")
        }
        async fn cancel_open_order(
            &self,
            _: &crate::exchange::OpenOrder,
        ) -> Result<(), ExchangeError> {
            unreachable!("账本不查挂单")
        }
        async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
            unreachable!("账本不查账户")
        }
    }

    fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        Arc::new(HashMap::from([(
            (EX, SYM.to_string()),
            SymbolMeta {
                exchange: EX,
                symbol: SYM.to_string(),
                price_rule: Arc::new(PriceStep::new(dec!(0.1))),
                size_step: dec!(0.001),
                min_order_size: dec!(0.001),
                min_notional: 0.0,
                contract_size: dec!(1),
            },
        )]))
    }

    fn baseline(size: Decimal) -> PositionBaseline {
        PositionBaseline {
            position: Position {
                exchange: EX,
                symbol: SYM.to_string(),
                size,
            },
            snapshot_req_ts: 1,
        }
    }

    /// 在 `root` 的 consumers 组里起一个账本并完成投产注册（基线 2.0）
    async fn ledger(
        root: &Root,
        client: Arc<dyn AccountClient>,
        interval_ms: u64,
    ) -> PositionLedgerHandle {
        ledger_on(
            root,
            client,
            interval_ms,
            AccountBus::new("account"),
            MarketBus::new("market"),
        )
        .await
    }

    /// 同 [`ledger`]，总线由调用方给（要往里发事件或收账本发出的读数时用）
    async fn ledger_on(
        root: &Root,
        client: Arc<dyn AccountClient>,
        interval_ms: u64,
        account_bus: AccountBus,
        market_bus: MarketBus,
    ) -> PositionLedgerHandle {
        let (_child, ledger) = PositionLedger::spawn(
            &root.group("consumers"),
            PositionLedgerArgs {
                accounts: HashMap::from([(EX, client)]),
                account_bus,
                market_bus,
                interval_ms,
                max_consecutive_mismatches: DEFAULT_MAX_CONSECUTIVE_MISMATCHES,
            },
        )
        .await
        .expect("账本应能起来");
        ledger
            .register_symbols(vec![SYM.to_string()], vec![baseline(dec!(2.0))])
            .await
            .expect("注册应成功");
        ledger
    }

    /// 有界等待：条件成立即返回，否则 2 秒后失败
    async fn eventually(what: &str, cond: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("等不到：{what}"));
    }

    /// **回归防线（Critical）**：在途请求永不返回时，停摆守卫必须仍能判定致命。
    ///
    /// 守卫的两个触点都在"结果到达"的路径上。若节拍只是跳过在途的所而不过守卫，
    /// 请求真挂死会让该所对账**静默永久失效**，引擎继续无校验交易 ——
    /// 这比漏报单次漂移严重得多，因为它没有任何外在症状。
    #[test]
    fn an_in_flight_fetch_that_never_returns_still_trips_the_staleness_guard() {
        let (client, _calls) = FaultyClient::pair(Faulty::Hang);
        let mut ledger = Ledger {
            reconciler: Reconciler::new(DEFAULT_MAX_CONSECUTIVE_MISMATCHES),
            accounts: HashMap::from([(EX, client)]),
            guards: HashMap::from([(
                EX,
                StalenessGuard::new(format!("{EX} 持仓对账"), MAX_POLL_STALENESS_MS),
            )]),
            in_flight: HashSet::from([EX]),
            executors: HashMap::new(),
            executor_in_flight: HashSet::new(),
            pending: FuturesUnordered::new(),
            account_bus: AccountBus::new("account"),
            mids: HashMap::new(),
            machine_stop: Root::new("t").machine_stop_flag(),
        };

        // 刚发出、还在容忍窗口内：只是慢，不该停机
        ledger
            .check_in_flight_staleness()
            .expect("窗口内的在途请求只是慢，不该判致命");

        // 在途已超出容忍窗口 —— 与"拉取持续失败"同等对待
        ledger
            .guards
            .get_mut(&EX)
            .unwrap()
            .rewind(MAX_POLL_STALENESS_MS + 1);
        let err = ledger
            .check_in_flight_staleness()
            .expect_err("在途超窗必须致命，否则对账通道静默永久失效");
        assert!(err.contains("已停摆"), "got: {err}");
    }

    /// **回归防线**：拉取 panic 不能让结果凭空消失 —— 它必须上冒、让整机停下。
    ///
    /// 旧结构里拉取在别处 spawn、结果经消息回投，panic 会连回投一起带走，`in_flight` 永久
    /// 占位、该所对账静默失效（当时靠把 panic 翻译成一条 `Err` 回投兜住）。新结构里拉取在
    /// 本任务里被 poll，panic 必然随 `run` 上冒：这里钉住这一点。
    #[tokio::test]
    async fn a_panicking_fetch_halts_the_ledger() {
        let root = Root::new("t");
        let (client, _calls) = FaultyClient::pair(Faulty::Panic);
        let _ledger = ledger(&root, client, NEVER_AGAIN_MS).await;
        tokio::time::timeout(Duration::from_secs(2), root.stopping())
            .await
            .expect("拉取 panic 必须上冒到根");
        let err = root.shutdown().await.expect_err("应带着错误退出");
        assert!(
            format!("{err:#}").contains("模拟 adapter 内的 panic"),
            "{err:#}"
        );
    }

    /// **回归防线**：REST 拉取卡死时，收件箱照常处理（Fill 摄入、投产注册都在这里）。
    ///
    /// 拉取一旦回到收件箱处理里 `await`（改动之前的形态），这次注册会永远排在那次网络
    /// 超时后面，测试超时失败。
    #[tokio::test]
    async fn a_hung_rest_fetch_never_blocks_the_inbox() {
        let root = Root::new("t");
        let (client, calls) = FaultyClient::pair(Faulty::Hang);
        let ledger = ledger(&root, client, NEVER_AGAIN_MS).await;
        // 首拍已发起一轮拉取（卡死在信号量上）
        eventually("首拍发起拉取", || calls.load(Ordering::SeqCst) >= 1).await;

        tokio::time::timeout(
            Duration::from_secs(2),
            ledger.register_symbols(vec!["ETH".to_string()], Vec::new()),
        )
        .await
        .expect("拉取卡死时收件箱必须仍能应答")
        .expect("应答应成功");
        root.shutdown().await.expect("正常停机");
    }

    /// 收账户总线上账本发出的持仓价值读数
    struct ValuesProbe {
        inbox: Inbox<AccountEvent>,
        seen: Arc<std::sync::Mutex<Vec<PositionValues>>>,
    }

    impl Task for ValuesProbe {
        async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
            while let Some(ev) = self.inbox.recv().await {
                if let AccountData::Exchange(ExchangeAccountData::PositionValues(v)) = ev.data {
                    self.seen.lock().unwrap().push(v);
                }
            }
            Ok(())
        }
    }

    /// #759 端到端：一笔成交后不等节拍，立刻按最新盘口发出该所的持仓价值
    #[tokio::test]
    async fn values_are_published_right_after_a_fill() {
        let root = Root::new("t");
        let (account_bus, market_bus) = (AccountBus::new("account"), MarketBus::new("market"));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe_seen = seen.clone();
        let probe_bus = account_bus.clone();
        root.group("consumers")
            .spawn_addressed(
                "ValuesProbe",
                move |_scope, mut inbox: Inbox<AccountEvent>| async move {
                    inbox.subscribe(
                        &probe_bus,
                        |e| {
                            matches!(
                                e.data,
                                AccountData::Exchange(ExchangeAccountData::PositionValues(_))
                            )
                        },
                        |e| e,
                    );
                    Ok(ValuesProbe {
                        inbox,
                        seen: probe_seen,
                    })
                },
            )
            .await
            .expect("探针应能起来");
        let (client, _calls) = FaultyClient::pair(Faulty::Steady(dec!(2.0)));
        // 节拍很慢：首拍（立即）之后的读数只可能来自成交
        let _ledger = ledger_on(
            &root,
            client,
            NEVER_AGAIN_MS,
            account_bus.clone(),
            market_bus.clone(),
        )
        .await;
        // 盘口与成交按序进账本收件箱
        market_bus.publish(MarketEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: MarketData::BBO(crate::domain::BBO {
                exchange: EX,
                symbol: SYM.to_string(),
                bid_price: dec!(99.0),
                bid_qty: dec!(1.0),
                ask_price: dec!(101.0),
                ask_qty: dec!(1.0),
                timestamp: 0,
            }),
        });
        let last = || seen.lock().unwrap().last().cloned();
        account_bus.publish(AccountEvent {
            account: crate::domain::AccountId::Live,
            exchange_ts: 2,
            local_ts: 2,
            data: AccountData::Symbol(crate::messaging::SymbolAccountData::Fill(
                crate::domain::Fill {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    side: crate::domain::Side::Long,
                    price: dec!(100.0),
                    size: dec!(1.0),
                    client_order_id: None,
                    order_id: "1".to_string(),
                    timestamp: 2,
                    fee: 0.0,
                    reason: crate::domain::FillReason::Normal,
                },
            )),
        });
        eventually("成交后立刻发出读数", || {
            last().is_some_and(|v| {
                v.exchange == EX && v.legs.get(SYM).map(LegValue::value) == Some(300.0)
            })
        })
        .await;
        root.shutdown().await.expect("正常停机");
    }

    /// 同一所在途期间不重复发起：慢 REST 不该堆积并发请求。
    #[tokio::test]
    async fn an_in_flight_exchange_is_not_fetched_again() {
        let root = Root::new("t");
        let (client, calls) = FaultyClient::pair(Faulty::Hang);
        let _ledger = ledger(&root, client, FAST_MS).await;
        eventually("首拍发起拉取", || calls.load(Ordering::SeqCst) >= 1).await;
        // 再过十来个节拍：若在途判据失效，多余的拉取会在此期间冒出来
        tokio::time::sleep(Duration::from_millis(FAST_MS * 10)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "在途期间的后续节拍必须跳过，否则慢 REST 会堆积并发请求"
        );
        root.shutdown().await.expect("正常停机");
    }

    // ---------------- executor 持仓纳入对账（#694） ----------------

    /// 只订一路 BBO、什么都不做的策略：本组用例只关心 executor 的持仓
    struct Idle;

    impl crate::strategy::Strategy for Idle {
        fn public_streams(&self) -> HashMap<Exchange, HashSet<crate::exchange::SubscriptionKind>> {
            HashMap::from([(
                EX,
                HashSet::from([crate::exchange::SubscriptionKind::BBO {
                    symbol: SYM.to_string(),
                }]),
            )])
        }
        fn on_event(
            &mut self,
            _event: &crate::messaging::IncomeEvent,
            _view: crate::strategy::StrategyView<'_>,
        ) -> Vec<crate::strategy::Signal> {
            Vec::new()
        }
    }

    /// 在 `root` 的 producers 组里起一个实盘 executor，出生持仓 = `size`
    /// （模拟「同一 symbol 再次投产时 executor 用了新快照」）
    async fn executor_born_with(root: &Root, size: Decimal) -> (ChildHandle, ExecutorHandle) {
        Executor::spawn(
            &root.group("producers"),
            ExecutorArgs {
                scope: crate::engine::subscription_scope(
                    AccountId::Live,
                    &crate::strategy::Strategy::public_streams(&Idle),
                ),
                strategy: Box::new(Idle),
                binding: crate::engine::live::AccountBinding::Live(
                    crate::engine::live::LiveBaselines::for_test(vec![baseline(size)]),
                ),
                symbol_metas: metas(),
                client_order_ids: Default::default(),
                outcome_bus: OutcomeBus::new("outcome"),
                metrics: super::super::Metrics::spawn_for_test(&root.group("consumers")).await,
            },
        )
        .await
        .expect("executor 应能起来")
    }

    /// 账本基线 2.0、交易所也报 2.0（这条比对永远通过），节拍很快
    async fn ledger_agreeing_with_exchange(root: &Root) -> PositionLedgerHandle {
        let (client, _calls) = FaultyClient::pair(Faulty::Steady(dec!(2.0)));
        ledger(root, client, FAST_MS).await
    }

    /// 足够跑完 N+2 轮「查询 -> 答复 -> 比对」的时长
    fn enough_rounds() -> Duration {
        Duration::from_millis(FAST_MS * (DEFAULT_MAX_CONSECUTIVE_MISMATCHES as u64 + 2) * 3)
    }

    /// **端到端**：交易所与账本都对，只有受监视的 executor 持仓错 —— 账本必须停机。
    #[tokio::test]
    async fn a_watched_executor_with_wrong_positions_halts_the_ledger() {
        let root = Root::new("t");
        let (_child, executor) = executor_born_with(&root, dec!(5.0)).await;
        let ledger = ledger_agreeing_with_exchange(&root).await;
        ledger.watch(executor).await.expect("纳入名单应成功");

        tokio::time::timeout(Duration::from_secs(2), root.stopping())
            .await
            .expect("executor 持仓与账本稳定分叉，账本必须致命退出");
        let err = root.shutdown().await.expect_err("应带着错误退出");
        assert!(
            format!("{err:#}").contains("executor"),
            "报错要指明对手方: {err:#}"
        );
    }

    /// 撤出名单的 executor 不再比对（撤下流程里它会先停更再停机，不能被误判）；
    /// 持仓一致的 executor 也不该触发任何东西。
    #[tokio::test]
    async fn unwatched_or_agreeing_executors_never_halt_the_ledger() {
        let root = Root::new("t");
        let (_c1, wrong) = executor_born_with(&root, dec!(5.0)).await;
        let (_c2, right) = executor_born_with(&root, dec!(2.0)).await;
        let ledger = ledger_agreeing_with_exchange(&root).await;
        let wrong_id = wrong.id();
        ledger.watch(wrong).await.unwrap();
        ledger.watch(right).await.unwrap();
        ledger.unwatch(wrong_id).await.unwrap();

        tokio::time::sleep(enough_rounds()).await;
        assert!(
            !root.is_stopping(),
            "撤出名单的、或持仓一致的 executor 都不该打死账本"
        );
        root.shutdown().await.expect("正常停机");
    }

    /// executor 停了却没撤出名单：查询得到 Gone -> 摘掉，不停机（它若是出错停的，
    /// 错误由它自己上冒）
    #[tokio::test]
    async fn a_stopped_executor_is_dropped_from_the_watch_list() {
        let root = Root::new("t");
        let (child, executor) = executor_born_with(&root, dec!(5.0)).await;
        let ledger = ledger_agreeing_with_exchange(&root).await;
        // 先停再纳入：确定地模拟「停了却还在名单里」，不依赖停机与节拍的先后
        child.stop().await;
        ledger.watch(executor).await.unwrap();

        tokio::time::sleep(enough_rounds()).await;
        assert!(!root.is_stopping(), "已停的 executor 不该被当成分叉");
        root.shutdown().await.expect("正常停机");
    }
}

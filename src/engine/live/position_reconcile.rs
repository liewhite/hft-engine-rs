//! PositionReconcileActor —— 持仓对账（本地「基线 + Fill」 vs 交易所读数）。
//!
//! 持仓的权威值由「启动期 REST 基线 + 之后全程 Fill 累加」维护
//! （见 [`ExchangeEventData::PositionBaseline`]）。这条通道会错：Fill 可能丢、可能被重复
//! 计算、codec 可能算错单位或符号、也可能出现不伴随 Fill 的交易所侧变动。本 actor 是**独立
//! 的第二条通道**，用周期性 REST 读数校验第一条：
//!
//! ```text
//!   通道 A（权威）  基线 ──> Fill 累加 ──> 策略看到的持仓
//!                                │
//!   通道 B（校验）  REST 轮询 ──> PositionReport ──> 比对 ──> 连续 N 次不一致即致命退出
//! ```
//!
//! # 职责边界：为什么不放进 MetricsActor
//!
//! `MetricsActor` 明确声明"观测组件故障不得拖垮交易"，连定时器挂掉都不 kill 自己。而对账
//! 连续失败要**致命退出**，那是风控语义而非观测语义。混在一起会破坏它已经写明的边界。
//!
//! # 比对的是「事件流镜像」而非「各 executor 的内部状态」
//!
//! 本 actor 自建一份镜像（复用 [`StateManager`]，与策略侧同一份实现），由同一条 income 流
//! 的 `PositionBaseline` + `Fill` 驱动。因此它证明的是"**事件流**与交易所一致"，而不是
//! "某个 executor 的 StateManager 与交易所一致"。这个范围是有意的：executor 是同一事件流的
//! 确定性消费者，流对了它就对了；反过来若去 `ask` 每个 executor 的内部状态，就把风控耦合
//! 到了 executor 的生命周期上（晋升/降级会不断增删实例）。
//!
//! 镜像只吃 `PositionBaseline` 与 `Fill` —— 这两类正是通道 A 的全部输入，多喂无益。
//! 共享总线上的私有事件必属实盘账户（模拟账户走 `PaperPubSub`），故镜像天然是实盘持仓。

use crate::domain::{Exchange, Position, Symbol, SymbolMeta};
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::RegisterSymbols;

/// 连续多少次**差值稳定**的不一致才判定为真实漂移，见 [`MismatchRecord`]。
///
/// 不能取 1：REST 快照的时点与本地 Fill 流之间必然有间隙（成交已落交易所、Fill 稍后才到），
/// 单次不一致是正常的飞行窗口。
pub const DEFAULT_MAX_CONSECUTIVE_MISMATCHES: u32 = 3;

/// 观测层不做订单超时清理（镜像只关心持仓）
const NO_ORDER_TIMEOUT: u64 = 0;

/// 单条腿缓冲的 Fill 达到该数量即告警（见 `Reconciler::pending_fills`）
const FILL_BUFFER_WARN_LEN: usize = 256;

// ============================================================================
// 纯逻辑核心
// ============================================================================

/// 对账纯逻辑：喂事件、维护镜像、比对读数。不含任何 actor / 时钟 / IO，可同步单测。
///
/// 与 [`crate::engine::StrategyRunner`] 同样的分层：纯核心 + 薄 actor 包装。
pub struct Reconciler {
    /// 镜像持仓：由 `PositionBaseline` + `Fill` 驱动（复用策略侧同一实现）。
    ///
    /// 同时充当"本实例负责哪些 symbol"的单一数据源：symbol 是否已注册直接问它，
    /// 不另存一份集合。分桶部署下多实例共用同一账户，交易所读数是**账户级全量**、
    /// 含其他桶的 symbol，靠这个判据过滤。
    mirror: StateManager,
    /// **已收到基线**的 (所, symbol)，即对账的有效范围。
    ///
    /// 收到基线前，本地持仓是"未知"而不是"0"——拿未知去比对必然误判。启动期
    /// `ManagerActor` 要为每个 (所, symbol) 逐个拉 REST、推基线，期间读数已经在流动；
    /// 若按 0 比对，几百个 symbol 的启动过程就会被判成大面积漂移而误停机。
    ///
    /// 它同时把对账范围精确到"策略真正订阅的所"：某 symbol 只在 Binance 上市时，
    /// `(OKX, 该 symbol)` 不会有基线，也就不该参与 OKX 的对账。
    baselined: HashSet<(Exchange, Symbol)>,
    /// 容差来源：`coin_size_step()`（币本位最小可交易增量），小于它的差值不可能是真实漂移
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// per (所, symbol) 的不一致记录；一致一次即整条移除
    mismatches: HashMap<(Exchange, Symbol), MismatchRecord>,
    max_consecutive: u32,
    /// 已注册、但**基线未到**的 (所, symbol) 上早到的 Fill，基线落地后重放。
    ///
    /// 投产时序是「注册对账范围 → REST 拉基线 → 经总线发布」，而私有成交流全程在推。
    /// 落在这个窗口里的 Fill 若直接入镜像，会在 positions 里凭空建出一条仓位，随后到达
    /// 的基线被判"重复"而丢弃 —— 镜像从此缺掉全部存量，对账在 N 次轮询后**误杀整个引擎**。
    /// 先缓冲、基线落地后重放，成交一笔不丢也不早算。
    ///
    /// # 重放按快照请求时刻过滤，缓冲因此可以安全地跨越失败的投产
    ///
    /// 基线事件的 `exchange_ts` 是快照的**请求**时刻（见 `ManagerActor::activate_executors`
    /// 6.2）。在此之前送达（`local_ts <= exchange_ts`）的 Fill，其成交必然已含在快照里，
    /// 重放会双计 —— 丢弃；之后送达的才补进镜像。这同时覆盖了两种来路的陈旧缓冲：
    /// - 正常投产窗口内、快照请求之前送达的 Fill；
    /// - **上一次投产失败**滞留的 Fill（Supervisor 晋升失败会在下个节拍重试，注册范围
    ///   有意不回滚）—— 重试成功时的新快照必然晚于它们，全部被过滤。
    ///
    /// 残余窗口如实说明：成交发生在快照请求之后、REST 在途期间，其 Fill 送达时已含在
    /// 快照里 —— 重放会多算一次。窗口只有 REST 在途时长，且只可能来自该腿的外部/手动
    /// 成交（此刻它没有 live 实例在交易）。没有交易所侧序号无法根除。
    pending_fills: HashMap<(Exchange, Symbol), Vec<IncomeEvent>>,
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
    last_diff: f64,
}

impl Reconciler {
    pub fn new(
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        max_consecutive: u32,
    ) -> Self {
        Self {
            mirror: StateManager::new(&[], NO_ORDER_TIMEOUT),
            baselined: HashSet::new(),
            symbol_metas,
            mismatches: HashMap::new(),
            max_consecutive,
            pending_fills: HashMap::new(),
        }
    }

    /// 注册要对账的 symbol。**必须先于持仓基线到达**，否则镜像里没有对应状态。
    pub fn register_symbols(&mut self, symbols: &[Symbol]) {
        self.mirror.register_symbols(symbols);
    }

    /// 该 symbol 是否属于本实例（镜像已注册即属于，见 `mirror` 字段说明）
    fn is_tracked(&self, symbol: &Symbol) -> bool {
        self.mirror.symbol_state(symbol).is_some()
    }

    /// 喂入一条 income 事件。
    ///
    /// `Err` = 已确认漂移（连续 `max_consecutive` 次超出容差），调用方应致命退出。
    pub fn on_event(&mut self, event: &IncomeEvent) -> Result<(), String> {
        match &event.data {
            ExchangeEventData::PositionReport {
                exchange,
                positions,
            } => self.reconcile(*exchange, positions),
            // 镜像的两个输入：基线与成交。其余事件与持仓无关，不喂（喂了还会因未注册的
            // symbol 触发 StateManager 的"路由 bug" error）。
            ExchangeEventData::PositionBaseline(position) => {
                if self.is_tracked(&position.symbol) {
                    let key = (position.exchange, position.symbol.clone());
                    self.mirror.apply(event);
                    // 基线到达才让这条腿进入对账范围（见 `baselined` 字段说明）
                    self.baselined.insert(key.clone());
                    // 重放基线之前早到的 Fill。快照请求时刻（event.exchange_ts）之前送达的
                    // 已含在快照里，重放即双计 —— 丢弃（见 `pending_fills` 字段说明）。
                    if let Some(buffered) = self.pending_fills.remove(&key) {
                        let (stale, fresh): (Vec<_>, Vec<_>) = buffered
                            .into_iter()
                            .partition(|f| f.local_ts <= event.exchange_ts);
                        tracing::info!(
                            exchange = %key.0,
                            symbol = %key.1,
                            replayed = fresh.len(),
                            dropped_as_covered_by_snapshot = stale.len(),
                            "基线落地，重放窗口期缓冲的 Fill"
                        );
                        for fill_event in &fresh {
                            self.mirror.apply(fill_event);
                        }
                    }
                }
                Ok(())
            }
            ExchangeEventData::Fill(fill) => {
                if self.is_tracked(&fill.symbol) {
                    let key = (fill.exchange, fill.symbol.clone());
                    if self.baselined.contains(&key) {
                        self.mirror.apply(event);
                    } else {
                        // 基线未到，此刻本地持仓是"未知"而非 0 —— 直接累加会让随后到达的
                        // 基线被判"重复"丢弃。缓冲到基线落地后重放。
                        tracing::info!(
                            exchange = %key.0,
                            symbol = %key.1,
                            size = fill.size,
                            "基线未到，Fill 先入缓冲"
                        );
                        let buf = self.pending_fills.entry(key.clone()).or_default();
                        buf.push(event.clone());
                        // 体量告警：基线迟迟不来（如晋升持续失败）而该腿又持续有外部成交。
                        // 陈旧条目会在基线落地时被过滤，这里只需让堆积可见；按阈值整倍数
                        // 节流，避免堆积场景下日志随 Fill 线性增长。
                        if buf.len() % FILL_BUFFER_WARN_LEN == 0 {
                            tracing::warn!(
                                exchange = %key.0,
                                symbol = %key.1,
                                buffered = buf.len(),
                                "基线长期未到，缓冲的 Fill 持续堆积"
                            );
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// 比对某所的一份完整读数。
    fn reconcile(&mut self, exchange: Exchange, positions: &[Position]) -> Result<(), String> {
        // 读数是该所的**完整**快照，因此"跟踪范围内、但读数里没有的 symbol"= 交易所侧空仓。
        // 这个推断正是选 REST 全量而非私有 WS 增量推送的理由（见 PositionReport 的文档）：
        // 它才能抓到最危险的一类漂移 —— 本地有仓、交易所已经没了。
        let reported: HashMap<&Symbol, f64> =
            positions.iter().map(|p| (&p.symbol, p.size)).collect();

        // 拆开字段以取得互不重叠的借用（baselined 只读、mismatches 可变）
        let Self {
            mirror,
            baselined,
            symbol_metas,
            mismatches,
            max_consecutive,
            // 读数比对不碰缓冲：基线未到的腿本就不在 baselined 里，不会被比对
            pending_fills: _,
        } = self;

        let mut confirmed_drift = Vec::new();

        // 只比对**该所**已收到基线的腿
        for symbol in baselined
            .iter()
            .filter(|(ex, _)| *ex == exchange)
            .map(|(_, symbol)| symbol)
        {
            let reported_size = reported.get(symbol).copied().unwrap_or(0.0);
            let local_size = mirror
                .symbol_state(symbol)
                .map(|s| s.position_size(exchange))
                .unwrap_or(0.0);
            let tolerance = tolerance_of(symbol_metas, exchange, symbol);
            // 带符号差值：+X 与 -X 是方向相反的两种漂移，不能混为一谈
            let diff = local_size - reported_size;
            let key = (exchange, symbol.clone());

            if diff.abs() <= tolerance {
                // 一致即整条移除：记录衡量的是"当前正持续着的不一致"，不是历史累计
                mismatches.remove(&key);
                continue;
            }

            // 差值是否与上次相同 —— 区分真漂移与在途成交，见 MismatchRecord
            let stable = mismatches
                .get(&key)
                .is_some_and(|prev| (diff - prev.last_diff).abs() <= tolerance);
            let streak = if stable {
                mismatches.get(&key).map_or(1, |prev| prev.streak + 1)
            } else {
                1
            };
            mismatches.insert(
                key,
                MismatchRecord {
                    streak,
                    last_diff: diff,
                },
            );

            // 打成结构化日志，让漂移成为可观测的趋势，而不是只有停机那一刻才被看到
            tracing::warn!(
                target: "metrics",
                %exchange,
                %symbol,
                local_size = format!("{local_size:.8}"),
                reported_size = format!("{reported_size:.8}"),
                diff = format!("{diff:.8}"),
                tolerance = format!("{tolerance:.8}"),
                stable_diff = stable,
                streak,
                max_consecutive = *max_consecutive,
                "metrics.position_drift：本地持仓与交易所读数不一致"
            );

            if streak >= *max_consecutive {
                confirmed_drift.push(format!(
                    "{exchange}/{symbol}: 本地 {local_size} vs 交易所 {reported_size} \
                     (差 {diff}, 容差 {tolerance})"
                ));
            }
        }

        if confirmed_drift.is_empty() {
            return Ok(());
        }
        Err(format!(
            "持仓对账连续 {} 次差值稳定的不一致，本地「基线 + Fill」已不可信: {}",
            self.max_consecutive,
            confirmed_drift.join("; ")
        ))
    }
}

/// 容差 = 该 symbol 的最小可交易**币量**增量（`size_step × contract_size`，见
/// [`SymbolMeta::coin_size_step`]）。
///
/// 比对双方（本地持仓与交易所读数）都是币本位，容差必须同口径。直接用 `size_step`
/// 是单位错误：它是**交易所下单单位**的步长 —— OKX BTC-USDT-SWAP（`size_step=1` 张、
/// `contract_size=0.01`）会把容差放大成 1.0 BTC，反之 `contract_size > 1` 的品种会
/// 过紧而误停机。
///
/// 缺 meta 说明该 symbol 未在此所上市（那么两边都是 0，容差取多少都一样），退化为浮点
/// 误差量级。下界钉在 `Position::EPSILON`：步长再小也不该小于浮点噪声。
fn tolerance_of(
    symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
    exchange: Exchange,
    symbol: &Symbol,
) -> f64 {
    symbol_metas
        .get(&(exchange, symbol.clone()))
        .map(|meta| meta.coin_size_step())
        .unwrap_or(Position::EPSILON)
        .max(Position::EPSILON)
}

// ============================================================================
// Actor 包装
// ============================================================================

/// PositionReconcileActor 初始化参数
pub struct PositionReconcileArgs {
    /// 用于取 `coin_size_step()`（币本位最小可交易增量）作为对账容差
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 连续多少次不一致判定为真实漂移
    pub max_consecutive_mismatches: u32,
}

/// 持仓对账 actor（薄包装，逻辑全在 [`Reconciler`]）
pub struct PositionReconcileActor {
    reconciler: Reconciler,
}

impl Actor for PositionReconcileActor {
    type Args = PositionReconcileArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::info!(
            max_consecutive_mismatches = args.max_consecutive_mismatches,
            "PositionReconcileActor started"
        );
        Ok(Self {
            reconciler: Reconciler::new(args.symbol_metas, args.max_consecutive_mismatches),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("PositionReconcileActor stopped");
        Ok(())
    }
}

impl Message<RegisterSymbols> for PositionReconcileActor {
    type Reply = ();

    async fn handle(&mut self, msg: RegisterSymbols, _ctx: &mut Context<Self, Self::Reply>) {
        tracing::info!(count = msg.0.len(), "PositionReconcileActor tracking symbols");
        self.reconciler.register_symbols(&msg.0);
    }
}

impl Message<IncomeEvent> for PositionReconcileActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, ctx: &mut Context<Self, Self::Reply>) {
        if let Err(reason) = self.reconciler.on_event(&msg) {
            // 本地持仓已不可信：策略的三道风控闸门（单边杠杆 / 账户杠杆 / 仓位上限）全部
            // 基于它计算，继续跑等于无风控裸奔。受控退出交人工介入。
            tracing::error!(
                %reason,
                "持仓对账确认漂移，退出（本地持仓不可信，风控闸门已失效，需人工核对后重启）"
            );
            ctx.actor_ref().kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fill, FillReason, Side};
    use crate::exchange::utils::StepFormatter;

    const EX: Exchange = Exchange::Binance;
    const OTHER_EX: Exchange = Exchange::OKX;
    const SYM: &str = "BTC";
    const SIZE_STEP: f64 = 0.001;

    fn metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        Arc::new(
            [EX, OTHER_EX]
                .into_iter()
                .map(|exchange| {
                    (
                        (exchange, SYM.to_string()),
                        SymbolMeta {
                            exchange,
                            symbol: SYM.to_string(),
                            price_formatter: Arc::new(StepFormatter::new(0.1)),
                            size_step: SIZE_STEP,
                            min_order_size: SIZE_STEP,
                            contract_size: 1.0,
                        },
                    )
                })
                .collect(),
        )
    }

    fn reconciler(max_consecutive: u32) -> Reconciler {
        let mut r = Reconciler::new(metas(), max_consecutive);
        r.register_symbols(&[SYM.to_string()]);
        r
    }

    fn ev(data: ExchangeEventData) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 1,
            local_ts: 1,
            data,
        }
    }

    /// 改写事件的时间戳（基线的 exchange_ts = 快照请求时刻，Fill 的 local_ts = 送达时刻）
    fn at(mut event: IncomeEvent, exchange_ts: u64, local_ts: u64) -> IncomeEvent {
        event.exchange_ts = exchange_ts;
        event.local_ts = local_ts;
        event
    }

    fn position(exchange: Exchange, size: f64) -> Position {
        Position {
            exchange,
            symbol: SYM.to_string(),
            size,
            entry_price: 100.0,
            unrealized_pnl: 0.0,
        }
    }

    fn baseline(size: f64) -> IncomeEvent {
        baseline_on(EX, size)
    }

    fn baseline_on(exchange: Exchange, size: f64) -> IncomeEvent {
        ev(ExchangeEventData::PositionBaseline(position(exchange, size)))
    }

    fn fill(side: Side, size: f64) -> IncomeEvent {
        fill_on(EX, side, size)
    }

    fn fill_on(exchange: Exchange, side: Side, size: f64) -> IncomeEvent {
        ev(ExchangeEventData::Fill(Fill {
            exchange,
            symbol: SYM.to_string(),
            side,
            price: 100.0,
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: 1,
            fee: 0.0,
            reason: FillReason::Normal,
        }))
    }

    /// 一份读数（该所完整快照）
    fn report(exchange: Exchange, positions: Vec<Position>) -> IncomeEvent {
        ev(ExchangeEventData::PositionReport {
            exchange,
            positions,
        })
    }

    #[test]
    fn matching_position_is_not_drift() {
        let mut r = reconciler(3);
        r.on_event(&baseline(2.0)).unwrap();
        r.on_event(&fill(Side::Long, 1.0)).unwrap();
        // 本地 3.0，交易所也 3.0
        for _ in 0..10 {
            r.on_event(&report(EX, vec![position(EX, 3.0)])).unwrap();
        }
    }

    /// 差值在 size_step 之内不算漂移（浮点累加噪声不该触发停机）
    #[test]
    fn diff_within_size_step_is_tolerated() {
        let mut r = reconciler(1);
        r.on_event(&baseline(1.0)).unwrap();
        let almost = 1.0 + SIZE_STEP * 0.5;
        r.on_event(&report(EX, vec![position(EX, almost)]))
            .expect("size_step 之内不应判为漂移");
    }

    /// **容差必须是币本位口径**（`size_step × contract_size`），不能直接用 `size_step`。
    ///
    /// OKX BTC-USDT-SWAP：`size_step=1`（张）、`contract_size=0.01`（币/张）——
    /// 币本位的最小增量是 0.01 BTC。若把 `size_step` 当币量用，容差会被放大成 1.0 BTC，
    /// 0.5 BTC 的真实漂移就被静默放过。
    #[test]
    fn tolerance_is_coin_denominated_not_raw_size_step() {
        // 每张 0.01 币、数量步长 1 张 -> 币本位最小增量 0.01
        let metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>> = Arc::new(HashMap::from([(
            (EX, SYM.to_string()),
            SymbolMeta {
                exchange: EX,
                symbol: SYM.to_string(),
                price_formatter: Arc::new(StepFormatter::new(0.1)),
                size_step: 1.0,
                min_order_size: 1.0,
                contract_size: 0.01,
            },
        )]));
        let mut r = Reconciler::new(metas, 1);
        r.register_symbols(&[SYM.to_string()]);
        r.on_event(&baseline(1.0)).unwrap();

        // 差 0.005 币 < 0.01 币：在币本位最小增量之内，是噪声
        r.on_event(&report(EX, vec![position(EX, 1.005)]))
            .expect("小于一个币本位最小增量的差值不该判为漂移");
        // 差 0.5 币 >> 0.01 币：真实漂移。若容差被错当成 1.0（裸 size_step），这里会漏检
        r.on_event(&report(EX, vec![position(EX, 1.5)]))
            .expect_err("0.5 币的差值远超币本位最小增量，必须判为漂移");
    }

    /// 连续 N 次超出容差才致命；第 N 次才返回 Err
    #[test]
    fn drift_is_fatal_only_after_n_consecutive_mismatches() {
        let mut r = reconciler(3);
        r.on_event(&baseline(1.0)).unwrap();

        r.on_event(&report(EX, vec![position(EX, 5.0)])).unwrap();
        r.on_event(&report(EX, vec![position(EX, 5.0)])).unwrap();
        let err = r
            .on_event(&report(EX, vec![position(EX, 5.0)]))
            .expect_err("连续第 3 次不一致应致命");
        assert!(err.contains("连续 3 次差值稳定的不一致"), "got: {err}");
    }

    /// **误停机防线**：差值每次都在变 = 在途成交造成的错位，不是固定漂移，不该累加。
    ///
    /// 某 symbol 持续成交时，每次轮询都可能正好撞在"成交已落交易所、Fill 尚未送达"的窗口
    /// 上。只数次数会把它攒成致命 —— 而误停机比漏报更糟。
    #[test]
    fn changing_diff_never_accumulates_into_a_fatal() {
        let mut r = reconciler(3);
        r.on_event(&baseline(1.0)).unwrap();

        // 本地始终 1.0，交易所读数每次不同 -> 差值每次不同 -> 永远停在 streak=1
        for reported in [3.0, 7.0, 2.5, 9.0, 4.25, 6.5, 8.75] {
            r.on_event(&report(EX, vec![position(EX, reported)]))
                .expect("差值在变说明是在途成交，不该判为漂移");
        }
    }

    /// 反面：真漂移的差值**恒定**，即使本地持仓一直在变也能检出。
    ///
    /// 漏算一笔 X 之后，本地与交易所会同步移动，差值仍是 X —— 这正是"差值稳定"判据既不
    /// 误伤飞行窗口、又不会在活跃交易期压制真实漂移的原因。
    #[test]
    fn constant_diff_is_detected_even_while_position_keeps_moving() {
        let mut r = reconciler(3);
        r.on_event(&baseline(10.0)).unwrap();

        // 交易所比本地少 2（漏算了一笔 -2 的成交），此后持续成交、两边同步移动
        r.on_event(&report(EX, vec![position(EX, 8.0)])).unwrap();
        r.on_event(&fill(Side::Long, 1.0)).unwrap(); // 本地 11
        r.on_event(&report(EX, vec![position(EX, 9.0)])).unwrap(); // 差值仍 +2
        r.on_event(&fill(Side::Long, 3.0)).unwrap(); // 本地 14
        let err = r
            .on_event(&report(EX, vec![position(EX, 12.0)])) // 差值仍 +2
            .expect_err("差值恒定即真漂移，持续交易不该把它抹掉");
        assert!(err.contains("本地 14"), "got: {err}");
    }

    /// 差值反向也算变化：+X 与 -X 是方向相反的两种漂移，不能接续计数
    #[test]
    fn sign_flip_in_diff_restarts_the_streak() {
        let mut r = reconciler(2);
        r.on_event(&baseline(5.0)).unwrap();

        // 差值 -3（交易所多）
        r.on_event(&report(EX, vec![position(EX, 8.0)])).unwrap();
        // 差值 +3（本地多）—— 绝对值相同但方向相反，必须重新从 1 数起
        r.on_event(&report(EX, vec![position(EX, 2.0)]))
            .expect("差值反向不该接着上一次的计数");
        // 再来一次 +3 才够 2 次
        let err = r
            .on_event(&report(EX, vec![position(EX, 2.0)]))
            .expect_err("同方向连续两次应致命");
        assert!(err.contains("交易所 2"), "got: {err}");
    }

    /// **关键**：中间只要一致一次就清零，计数器衡量的是"持续不一致"而非历史累计。
    /// 否则飞行窗口（成交已落交易所、Fill 稍后才到）攒够次数就会误停机。
    #[test]
    fn a_single_match_resets_the_streak() {
        let mut r = reconciler(3);
        r.on_event(&baseline(1.0)).unwrap();

        r.on_event(&report(EX, vec![position(EX, 5.0)])).unwrap();
        r.on_event(&report(EX, vec![position(EX, 5.0)])).unwrap();
        // 一致一次 -> 清零
        r.on_event(&report(EX, vec![position(EX, 1.0)])).unwrap();
        // 再来两次不一致仍不该致命（重新从 1 数起）
        r.on_event(&report(EX, vec![position(EX, 5.0)])).unwrap();
        r.on_event(&report(EX, vec![position(EX, 5.0)]))
            .expect("清零后只累计到 2，不应致命");
    }

    /// **最危险的一类漂移**：本地有仓、交易所读数里根本没有这个 symbol。
    ///
    /// 读数是完整快照，"没报告"就等于空仓；若按"没报告就跳过"处理，漏掉的正好是
    /// 平仓 Fill 丢失 / 强平回报丢失这两种最该抓的情况。
    #[test]
    fn local_position_absent_from_report_is_drift() {
        let mut r = reconciler(1);
        r.on_event(&baseline(2.0)).unwrap();

        let err = r
            .on_event(&report(EX, vec![]))
            .expect_err("本地有仓而读数为空必须判为漂移");
        assert!(err.contains("交易所 0"), "got: {err}");
    }

    /// 反向：本地空仓、交易所却有仓（漏了一笔开仓 Fill）。
    ///
    /// 基线本身是 0（`ManagerActor` 对交易所未返回的 symbol 会显式推 size=0），此后交易所
    /// 冒出仓位就说明漏了成交。
    #[test]
    fn unknown_exchange_position_is_drift() {
        let mut r = reconciler(1);
        r.on_event(&baseline(0.0)).unwrap();
        let err = r
            .on_event(&report(EX, vec![position(EX, 1.5)]))
            .expect_err("本地空仓而交易所有仓必须判为漂移");
        assert!(err.contains("本地 0"), "got: {err}");
    }

    /// **基线未到之前不对账**：那时本地持仓是"未知"，不是 0。
    ///
    /// 启动期 `ManagerActor` 要为每个 (所, symbol) 逐个拉 REST 再推基线，期间读数已经在
    /// 流动；若按 0 比对，几百个 symbol 的启动过程会被判成大面积漂移而误停机。
    #[test]
    fn no_reconciliation_before_baseline_arrives() {
        let mut r = reconciler(1);
        // 已注册 symbol，但还没收到基线 —— 交易所报了个大仓位也不该判漂移
        for _ in 0..5 {
            r.on_event(&report(EX, vec![position(EX, 123.0)]))
                .expect("基线未到时不应对账");
        }
        // 基线到达后才开始比对
        r.on_event(&baseline(123.0)).unwrap();
        r.on_event(&report(EX, vec![position(EX, 123.0)]))
            .expect("基线与读数一致");
        let err = r
            .on_event(&report(EX, vec![position(EX, 0.0)]))
            .expect_err("基线之后出现真实分歧才该报");
        assert!(err.contains("本地 123"), "got: {err}");
    }

    /// **投产窗口竞态**：私有流的 Fill 抢在基线之前到达时先缓冲，基线落地后重放。
    ///
    /// 若不缓冲，早到的 Fill 会在镜像里凭空建出仓位，随后的基线被判"重复"丢弃 ——
    /// 镜像缺掉全部存量，对账在 N 次轮询后误杀整个引擎。
    #[test]
    fn fill_before_baseline_is_buffered_and_replayed() {
        let mut r = reconciler(1);
        // Fill 先到（此刻本地持仓是"未知"，不是 0）；t=2 送达，晚于快照请求时刻 t=1
        r.on_event(&at(fill(Side::Long, 1.0), 2, 2)).unwrap();
        // 基线随后到达，不该被判"重复"；缓冲的 Fill 补上 -> 本地 = 2.0 + 1.0
        r.on_event(&baseline(2.0)).unwrap();
        r.on_event(&report(EX, vec![position(EX, 3.0)]))
            .expect("基线 + 重放 Fill 应与交易所读数一致");
        // 反证本地确实是 3.0：给一个只含基线的读数，必须立刻判为漂移
        let err = r
            .on_event(&report(EX, vec![position(EX, 2.0)]))
            .expect_err("若 Fill 被丢弃本地会是 2.0，这里就不会报漂移");
        assert!(err.contains("本地 3"), "got: {err}");
    }

    /// 缓冲按 (所, symbol) 隔离：一条腿的基线落地只重放**自己**的 Fill
    #[test]
    fn buffered_fills_are_scoped_to_their_leg() {
        let mut r = reconciler(1);
        // OKX 腿的 Fill 早到（其基线未到）；t=2 送达，晚于快照请求时刻 t=1
        r.on_event(&at(fill_on(OTHER_EX, Side::Long, 5.0), 2, 2)).unwrap();
        // Binance 腿基线落地，不该动 OKX 的缓冲
        r.on_event(&baseline(1.0)).unwrap();
        r.on_event(&report(EX, vec![position(EX, 1.0)]))
            .expect("Binance 腿只有自己的基线");
        // OKX 腿基线落地后，它的缓冲才重放
        r.on_event(&baseline_on(OTHER_EX, 0.0)).unwrap();
        r.on_event(&report(OTHER_EX, vec![position(OTHER_EX, 5.0)]))
            .expect("OKX 腿 = 基线 0 + 重放的 5.0");
    }

    /// **重放过滤**：快照请求时刻（基线 exchange_ts）之前送达的缓冲 Fill 已含在快照里，
    /// 重放即双计 —— 必须丢弃。
    ///
    /// 典型来路：上一次投产失败后滞留的缓冲（Supervisor 晋升失败会在下个节拍重试，
    /// 对账范围注册有意不回滚）。若不过滤，重试成功后镜像 = 快照(已含该笔) + 重放(再加
    /// 一次)，恒定偏差 → 对账误杀引擎。
    #[test]
    fn replay_drops_fills_already_covered_by_snapshot() {
        let mut r = reconciler(1);
        // t=5 送达一笔 Fill（此后它会体现在快照里）
        r.on_event(&at(fill(Side::Long, 1.0), 5, 5)).unwrap();
        // t=12 又送达一笔，尚不在快照里
        r.on_event(&at(fill(Side::Long, 4.0), 12, 12)).unwrap();
        // t=10 请求的快照读到 2.0（已含 t=5 那笔），t=11 作为基线到达
        r.on_event(&at(baseline(2.0), 10, 11)).unwrap();
        // 镜像 = 2.0(快照) + 4.0(重放 t=12) = 6.0；t=5 那笔被过滤
        r.on_event(&report(EX, vec![position(EX, 6.0)]))
            .expect("快照已含的 Fill 不该重放");
        let err = r
            .on_event(&report(EX, vec![position(EX, 7.0)]))
            .expect_err("若 t=5 的 Fill 也被重放，本地会是 7.0 而这里不会报漂移");
        assert!(err.contains("本地 6"), "got: {err}");
    }

    /// 跟踪范围外的 symbol 不参与对账（分桶部署下账户级读数含其他桶的 symbol）
    #[test]
    fn untracked_symbols_are_ignored() {
        let mut r = reconciler(1);
        r.on_event(&baseline(1.0)).unwrap();
        let other_bucket = Position {
            exchange: EX,
            symbol: "ETH".to_string(),
            size: 42.0,
            entry_price: 1.0,
            unrealized_pnl: 0.0,
        };
        // 读数里混入其他桶的 symbol；本桶的 BTC 对得上，就不该有漂移
        r.on_event(&report(EX, vec![position(EX, 1.0), other_bucket]))
            .expect("其他桶的 symbol 不该触发漂移");
    }

    /// 计数按 (所, symbol) 分开：一个所的漂移不该借另一个所的次数达标
    #[test]
    fn streaks_are_per_exchange() {
        let mut r = reconciler(2);
        r.on_event(&baseline(1.0)).unwrap();
        r.on_event(&baseline_on(OTHER_EX, 0.0)).unwrap();

        // Binance 不一致一次
        r.on_event(&report(EX, vec![position(EX, 9.0)])).unwrap();
        // OKX 上本地与读数都是 0 -> 一致，不该影响 Binance 的计数
        r.on_event(&report(OTHER_EX, vec![])).unwrap();
        // Binance 第二次不一致 -> 达标
        let err = r
            .on_event(&report(EX, vec![position(EX, 9.0)]))
            .expect_err("同一个所连续两次应致命");
        assert!(err.contains("Binance"), "got: {err}");
    }

    /// 同一 symbol 跨所独立：Binance 漂了，OKX 那条腿的读数不受影响
    #[test]
    fn per_exchange_legs_are_compared_independently() {
        let mut r = reconciler(1);
        r.on_event(&baseline(1.0)).unwrap();
        r.on_event(&baseline_on(OTHER_EX, -1.0)).unwrap();

        // OKX 腿一致
        r.on_event(&report(OTHER_EX, vec![position(OTHER_EX, -1.0)]))
            .expect("OKX 腿对得上");
        // Binance 腿不一致
        let err = r
            .on_event(&report(EX, vec![position(EX, 4.0)]))
            .expect_err("Binance 腿应报漂移");
        assert!(err.contains("Binance"), "got: {err}");
        assert!(!err.contains("OKX"), "不该牵连对得上的 OKX 腿: {err}");
    }

    /// 镜像只吃基线与成交：对账读数本身绝不能写进镜像（否则漂移会被自我抹平）
    #[test]
    fn report_never_feeds_the_mirror() {
        let mut r = reconciler(2);
        r.on_event(&baseline(1.0)).unwrap();
        // 第一次不一致
        r.on_event(&report(EX, vec![position(EX, 7.0)])).unwrap();
        // 若读数写进了镜像，这次就会"一致"从而清零，永远等不到致命
        let err = r
            .on_event(&report(EX, vec![position(EX, 7.0)]))
            .expect_err("读数若写进镜像，漂移会被自我抹平");
        assert!(err.contains("本地 1"), "镜像被读数污染了: {err}");
    }
}

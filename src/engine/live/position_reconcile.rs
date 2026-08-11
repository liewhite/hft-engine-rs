//! PositionReconcileActor —— 持仓对账（本地「基线 + Fill」 vs 交易所读数）。
//!
//! 持仓的权威值由「投产期 REST 基线 + 之后全程 Fill 累加」维护
//! （见 [`crate::messaging::PositionBaseline`]）。这条通道会错：Fill 可能丢、可能被重复
//! 计算、codec 可能算错单位或符号、也可能出现不伴随 Fill 的交易所侧变动。本 actor 是**独立
//! 的第二条通道**，用周期性 REST 读数校验第一条：
//!
//! ```text
//!   通道 A（权威）  基线 ──> Fill 累加 ──> 策略看到的持仓
//!                                │
//!   通道 B（校验）  自行轮询 REST 读数 ──> 比对 ──> 连续 N 次差值稳定的不一致即致命退出
//! ```
//!
//! # 读数自产自销：轮询就在本 actor 里
//!
//! 读数（某所的**完整**持仓快照）只有一个消费者 —— 本 actor。它不是事件，不上总线：
//! 本 actor 持有各 authed 所的 client，按节拍并发拉取、就地比对。完整快照的意义在于
//! "交易所没报告这个 symbol ⇒ 它空仓"这个推断 —— 对账最要抓的一类漂移正是**本地有仓、
//! 交易所已经没了**（漏了平仓成交/强平回报），只有全量快照能发现它。
//!
//! # 职责边界：为什么不放进 MetricsActor
//!
//! `MetricsActor` 明确声明"观测组件故障不得拖垮交易"，连定时器挂掉都不 kill 自己。而对账
//! 连续失败要**致命退出**，那是风控语义而非观测语义。混在一起会破坏它已经写明的边界。
//!
//! # 比对的是「事件流镜像」而非「各 executor 的内部状态」
//!
//! 本 actor 自建一份镜像（复用 [`StateManager`]，与策略侧同一份实现），由投产握手的基线
//! + income 流的 `Fill` 驱动。因此它证明的是"**事件流**与交易所一致"，而不是"某个
//! executor 的 StateManager 与交易所一致"。executor 是同一事件流的确定性消费者、且与镜像
//! 用同一份 seed 语义与同一条防双计规则（见 [`SymbolState::seed_position`]）——流对了它
//! 就对了。
//!
//! # 注册与基线原子到达，无缓冲重放
//!
//! [`RegisterSymbols`] 一次携带 symbol 集合与它们的基线（与 executor 同一份快照）。
//! "已注册、基线未到"的窗口不存在，历史上为它建的 Fill 缓冲 + 按快照时刻过滤重放的
//! 机制随之删除；快照与 Fill 流的竞态由 seed 内置的时刻过滤统一兜住。

use crate::domain::{now_ms, Exchange, Position, Symbol, SymbolMeta};
use crate::exchange::staleness::{StalenessGuard, MAX_POLL_STALENESS_MS};
use crate::exchange::ExchangeClient;
use crate::messaging::{ExchangeEventData, IncomeEvent, PositionBaseline, StateManager};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

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

/// 观测层不做订单超时清理（镜像只关心持仓）
const NO_ORDER_TIMEOUT: u64 = 0;

// ============================================================================
// 纯逻辑核心
// ============================================================================

/// 对账纯逻辑：注册基线、跟 Fill、比对读数。不含任何 actor / 时钟 / IO，可同步单测。
///
/// 与 [`crate::engine::StrategyRunner`] 同样的分层：纯核心 + 薄 actor 包装。
pub struct Reconciler {
    /// 镜像持仓：投产握手 seed 基线 + income 流的 `Fill` 驱动（复用策略侧同一实现）。
    ///
    /// 同时充当两个判据的单一数据源，不另存副本：
    /// - "本实例负责哪些 symbol"：symbol 是否已注册直接问它。分桶部署下多实例共用同一
    ///   账户，交易所读数是**账户级全量**、含其他桶的 symbol，靠这个判据过滤。
    /// - "哪条腿可参与对账"：`is_position_seeded` —— 基线未到（该所未配置 client）时
    ///   本地持仓是"未知"而不是 0，拿未知去比对必然误判。
    mirror: StateManager,
    /// 容差来源：`coin_size_step()`（币本位最小可交易增量），小于它的差值不可能是真实漂移
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// per (所, symbol) 的不一致记录；一致一次即整条移除
    mismatches: HashMap<(Exchange, Symbol), MismatchRecord>,
    max_consecutive: u32,
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
            symbol_metas,
            mismatches: HashMap::new(),
            max_consecutive,
        }
    }

    /// 注册要对账的 symbol 并**原子**写入它们的持仓基线（投产握手）。
    ///
    /// 与 executor / 观测镜像同一份快照、同一 seed 语义（幂等，重复注册是再晋升的
    /// 正常形态）；快照与 Fill 流的竞态由 seed 内置的时刻过滤兜住。
    pub fn register(&mut self, symbols: &[Symbol], baselines: &[PositionBaseline]) {
        self.mirror.register_symbols(symbols);
        self.mirror.seed_positions(baselines);
    }

    /// 该 symbol 是否属于本实例（镜像已注册即属于，见 `mirror` 字段说明）
    fn is_tracked(&self, symbol: &Symbol) -> bool {
        self.mirror.symbol_state(symbol).is_some()
    }

    /// 喂入一条 income 事件（镜像只吃 Fill —— 通道 A 的增量输入就它一个）。
    pub fn on_event(&mut self, event: &IncomeEvent) {
        if let ExchangeEventData::Fill(fill) = &event.data {
            if self.is_tracked(&fill.symbol) {
                self.mirror.apply(event);
            }
        }
    }

    /// 比对某所的一份完整读数。
    ///
    /// `Err` = 已确认漂移（连续 `max_consecutive` 次差值稳定的不一致），调用方应致命退出。
    pub fn reconcile(&mut self, exchange: Exchange, positions: &[Position]) -> Result<(), String> {
        // 读数是该所的**完整**快照，因此"跟踪范围内、但读数里没有的 symbol"= 交易所侧空仓。
        // 这个推断正是选 REST 全量而非私有 WS 增量推送的理由（见模块文档）：
        // 它才能抓到最危险的一类漂移 —— 本地有仓、交易所已经没了。
        let reported: HashMap<&Symbol, f64> =
            positions.iter().map(|p| (&p.symbol, p.size)).collect();

        // 拆开字段以取得互不重叠的借用（mirror 只读、mismatches 可变）
        let Self {
            mirror,
            symbol_metas,
            mismatches,
            max_consecutive,
        } = self;

        let mut confirmed_drift = Vec::new();

        // 只比对**该所**基线已写入的腿（基线未到 = 本地未知，不是 0）
        for (symbol, state) in mirror.symbol_states() {
            if !state.is_position_seeded(exchange) {
                continue;
            }
            let reported_size = reported.get(symbol).copied().unwrap_or(0.0);
            let local_size = state.position_size(exchange);
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
// Actor 包装（含读数轮询）
// ============================================================================

/// PositionReconcileActor 初始化参数
pub struct PositionReconcileArgs {
    /// 各 **authed** 所的 REST client（读数的来源；无凭证的所没有持仓可言，不轮询）
    pub clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 用于取 `coin_size_step()`（币本位最小可交易增量）作为对账容差
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 轮询间隔（毫秒）
    pub interval_ms: u64,
    /// 连续多少次不一致判定为真实漂移
    pub max_consecutive_mismatches: u32,
}

/// 持仓对账 actor（比对逻辑在 [`Reconciler`]，本层负责轮询与停摆守卫）
pub struct PositionReconcileActor {
    reconciler: Reconciler,
    clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 停摆守卫（每所一个）：对账通道停摆等于风控失效，超过容忍窗口即致命。
    /// 判据用"距上次成功的时长"而非"连续失败次数"，改轮询间隔不会连带改变容忍窗口。
    guards: HashMap<Exchange, StalenessGuard>,
}

impl PositionReconcileActor {
    /// 拉一轮全部所的读数并比对。`Err` = 确认漂移或通道停摆过久，调用方应致命退出。
    async fn poll_and_reconcile(&mut self) -> Result<(), String> {
        // 并发拉取各所（单所 REST 挂起不拖累其它所的对账节拍）
        let fetches = self.clients.iter().map(|(exchange, client)| {
            let exchange = *exchange;
            let client = client.clone();
            async move { (exchange, client.fetch_positions().await) }
        });
        for (exchange, result) in futures_util::future::join_all(fetches).await {
            match result {
                Ok(positions) => {
                    self.guards
                        .get_mut(&exchange)
                        .expect("guard 与 client 同建")
                        .record_success();
                    tracing::debug!(%exchange, count = positions.len(), "Position report polled");
                    self.reconciler.reconcile(exchange, &positions)?;
                }
                Err(e) => {
                    // REST 抖动（超时、限流、5xx）是常态，单次失败只 warn；
                    // 长期停摆由 guard 判定致命
                    let guard = self
                        .guards
                        .get_mut(&exchange)
                        .expect("guard 与 client 同建");
                    guard.check_failure(&e)?;
                    tracing::warn!(
                        %exchange,
                        error = %e,
                        stale_ms = guard.stale_ms(),
                        "Failed to fetch positions for reconciliation, will retry"
                    );
                }
            }
        }
        Ok(())
    }
}

impl Actor for PositionReconcileActor {
    type Args = PositionReconcileArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        actor_ref.attach_stream(IntervalStream::new(tokio::time::interval(interval)), (), ());

        // 窗口从构造时刻起算，避免首次拉取前就被判成停摆
        let guards = args
            .clients
            .keys()
            .map(|exchange| {
                (
                    *exchange,
                    StalenessGuard::new(format!("{exchange} 持仓对账"), MAX_POLL_STALENESS_MS),
                )
            })
            .collect();

        tracing::info!(
            exchanges = args.clients.len(),
            interval_ms = args.interval_ms,
            max_consecutive_mismatches = args.max_consecutive_mismatches,
            "PositionReconcileActor started"
        );
        Ok(Self {
            reconciler: Reconciler::new(args.symbol_metas, args.max_consecutive_mismatches),
            clients: args.clients,
            guards,
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

/// 投产注册握手：symbol 集合 + 持仓基线原子到达（与 [`super::RegisterSymbols`] 同形）。
impl Message<super::RegisterSymbols> for PositionReconcileActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: super::RegisterSymbols,
        _ctx: &mut Context<Self, Self::Reply>,
    ) {
        tracing::info!(count = msg.symbols.len(), "PositionReconcileActor tracking symbols");
        self.reconciler.register(&msg.symbols, &msg.baselines);
    }
}

/// income 流入口：镜像只吃 Fill（经 SubscribeFilter 过滤，见 manager 装配）
impl Message<IncomeEvent> for PositionReconcileActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        self.reconciler.on_event(&msg);
    }
}

/// 轮询节拍：拉读数、比对；确认漂移或通道停摆即致命退出
impl Message<StreamMessage<Instant, (), ()>> for PositionReconcileActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                // 没有任何 authed 所时无事可做（引擎只接公共行情）
                if self.clients.is_empty() {
                    return;
                }
                let _ = now_ms(); // 保持与其它轮询 actor 相同的时间来源约定（读数不带时戳）
                if let Err(reason) = self.poll_and_reconcile().await {
                    // 本地持仓已不可信：策略的三道风控闸门（单边杠杆 / 账户杠杆 / 仓位上限）
                    // 全部基于它计算，继续跑等于无风控裸奔。受控退出交人工介入。
                    tracing::error!(
                        %reason,
                        "持仓对账确认漂移或通道停摆，退出（本地持仓不可信，风控闸门已失效，需人工核对后重启）"
                    );
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Position reconcile polling started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("Position reconcile polling stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
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
    /// 测试基线统一的快照请求时刻；需要累加的 Fill 应带更晚的 local_ts
    const SNAPSHOT_TS: u64 = 1;

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

    /// 注册 + seed 基线（投产握手），SYM 一个 symbol
    fn reconciler_with(max_consecutive: u32, baselines: &[PositionBaseline]) -> Reconciler {
        let mut r = Reconciler::new(metas(), max_consecutive);
        r.register(&[SYM.to_string()], baselines);
        r
    }

    fn position(exchange: Exchange, size: f64) -> Position {
        Position {
            exchange,
            symbol: SYM.to_string(),
            size,
        }
    }

    fn baseline_on(exchange: Exchange, size: f64) -> PositionBaseline {
        PositionBaseline {
            position: position(exchange, size),
            snapshot_req_ts: SNAPSHOT_TS,
        }
    }

    fn fill_on(exchange: Exchange, side: Side, size: f64, local_ts: u64) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: local_ts,
            local_ts,
            data: ExchangeEventData::Fill(Fill {
                exchange,
                symbol: SYM.to_string(),
                side,
                price: 100.0,
                size,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: local_ts,
                fee: 0.0,
                reason: FillReason::Normal,
            }),
        }
    }

    fn fill(side: Side, size: f64) -> IncomeEvent {
        fill_on(EX, side, size, SNAPSHOT_TS + 1)
    }

    #[test]
    fn matching_position_is_not_drift() {
        let mut r = reconciler_with(3, &[baseline_on(EX, 2.0)]);
        r.on_event(&fill(Side::Long, 1.0));
        // 本地 3.0，交易所也 3.0
        for _ in 0..10 {
            r.reconcile(EX, &[position(EX, 3.0)]).unwrap();
        }
    }

    /// 差值在 size_step 之内不算漂移（浮点累加噪声不该触发停机）
    #[test]
    fn diff_within_size_step_is_tolerated() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 1.0)]);
        let almost = 1.0 + SIZE_STEP * 0.5;
        r.reconcile(EX, &[position(EX, almost)])
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
        r.register(&[SYM.to_string()], &[baseline_on(EX, 1.0)]);

        // 差 0.005 币 < 0.01 币：在币本位最小增量之内，是噪声
        r.reconcile(EX, &[position(EX, 1.005)])
            .expect("小于一个币本位最小增量的差值不该判为漂移");
        // 差 0.5 币 >> 0.01 币：真实漂移。若容差被错当成 1.0（裸 size_step），这里会漏检
        r.reconcile(EX, &[position(EX, 1.5)])
            .expect_err("0.5 币的差值远超币本位最小增量，必须判为漂移");
    }

    /// 连续 N 次超出容差才致命；第 N 次才返回 Err
    #[test]
    fn drift_is_fatal_only_after_n_consecutive_mismatches() {
        let mut r = reconciler_with(3, &[baseline_on(EX, 1.0)]);

        r.reconcile(EX, &[position(EX, 5.0)]).unwrap();
        r.reconcile(EX, &[position(EX, 5.0)]).unwrap();
        let err = r
            .reconcile(EX, &[position(EX, 5.0)])
            .expect_err("连续第 3 次不一致应致命");
        assert!(err.contains("连续 3 次差值稳定的不一致"), "got: {err}");
    }

    /// **误停机防线**：差值每次都在变 = 在途成交造成的错位，不是固定漂移，不该累加。
    ///
    /// 某 symbol 持续成交时，每次轮询都可能正好撞在"成交已落交易所、Fill 尚未送达"的窗口
    /// 上。只数次数会把它攒成致命 —— 而误停机比漏报更糟。
    #[test]
    fn changing_diff_never_accumulates_into_a_fatal() {
        let mut r = reconciler_with(3, &[baseline_on(EX, 1.0)]);

        // 本地始终 1.0，交易所读数每次不同 -> 差值每次不同 -> 永远停在 streak=1
        for reported in [3.0, 7.0, 2.5, 9.0, 4.25, 6.5, 8.75] {
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
        let mut r = reconciler_with(3, &[baseline_on(EX, 10.0)]);

        // 交易所比本地少 2（漏算了一笔 -2 的成交），此后持续成交、两边同步移动
        r.reconcile(EX, &[position(EX, 8.0)]).unwrap();
        r.on_event(&fill(Side::Long, 1.0)); // 本地 11
        r.reconcile(EX, &[position(EX, 9.0)]).unwrap(); // 差值仍 +2
        r.on_event(&fill(Side::Long, 3.0)); // 本地 14
        let err = r
            .reconcile(EX, &[position(EX, 12.0)]) // 差值仍 +2
            .expect_err("差值恒定即真漂移，持续交易不该把它抹掉");
        assert!(err.contains("本地 14"), "got: {err}");
    }

    /// 差值反向也算变化：+X 与 -X 是方向相反的两种漂移，不能接续计数
    #[test]
    fn sign_flip_in_diff_restarts_the_streak() {
        let mut r = reconciler_with(2, &[baseline_on(EX, 5.0)]);

        // 差值 -3（交易所多）
        r.reconcile(EX, &[position(EX, 8.0)]).unwrap();
        // 差值 +3（本地多）—— 绝对值相同但方向相反，必须重新从 1 数起
        r.reconcile(EX, &[position(EX, 2.0)])
            .expect("差值反向不该接着上一次的计数");
        // 再来一次 +3 才够 2 次
        let err = r
            .reconcile(EX, &[position(EX, 2.0)])
            .expect_err("同方向连续两次应致命");
        assert!(err.contains("交易所 2"), "got: {err}");
    }

    /// **关键**：中间只要一致一次就清零，计数器衡量的是"持续不一致"而非历史累计。
    /// 否则飞行窗口（成交已落交易所、Fill 稍后才到）攒够次数就会误停机。
    #[test]
    fn a_single_match_resets_the_streak() {
        let mut r = reconciler_with(3, &[baseline_on(EX, 1.0)]);

        r.reconcile(EX, &[position(EX, 5.0)]).unwrap();
        r.reconcile(EX, &[position(EX, 5.0)]).unwrap();
        // 一致一次 -> 清零
        r.reconcile(EX, &[position(EX, 1.0)]).unwrap();
        // 再来两次不一致仍不该致命（重新从 1 数起）
        r.reconcile(EX, &[position(EX, 5.0)]).unwrap();
        r.reconcile(EX, &[position(EX, 5.0)])
            .expect("清零后只累计到 2，不应致命");
    }

    /// **最危险的一类漂移**：本地有仓、交易所读数里根本没有这个 symbol。
    ///
    /// 读数是完整快照，"没报告"就等于空仓；若按"没报告就跳过"处理，漏掉的正好是
    /// 平仓 Fill 丢失 / 强平回报丢失这两种最该抓的情况。
    #[test]
    fn local_position_absent_from_report_is_drift() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 2.0)]);

        let err = r
            .reconcile(EX, &[])
            .expect_err("本地有仓而读数为空必须判为漂移");
        assert!(err.contains("交易所 0"), "got: {err}");
    }

    /// 反向：本地空仓、交易所却有仓（漏了一笔开仓 Fill）。
    ///
    /// 基线本身是 0（`ManagerActor` 对交易所未返回的 symbol 会显式给 size=0 的基线），
    /// 此后交易所冒出仓位就说明漏了成交。
    #[test]
    fn unknown_exchange_position_is_drift() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 0.0)]);
        let err = r
            .reconcile(EX, &[position(EX, 1.5)])
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
            r.reconcile(EX, &[position(EX, 123.0)])
                .expect("基线未写入的腿不应对账");
        }
        // 基线写入后才开始比对
        r.register(&[SYM.to_string()], &[baseline_on(EX, 123.0)]);
        r.reconcile(EX, &[position(EX, 123.0)])
            .expect("基线与读数一致");
        let err = r
            .reconcile(EX, &[position(EX, 0.0)])
            .expect_err("基线之后出现真实分歧才该报");
        assert!(err.contains("本地 123"), "got: {err}");
    }

    /// **防双计**：快照请求时刻之前送达的 Fill 已含在快照里，seed 后到达必须被过滤。
    /// （这条规则在 SymbolState::seed_position 内置，镜像与 executor 同一份。）
    #[test]
    fn fills_covered_by_snapshot_are_not_double_counted() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 2.0)]);
        // local_ts == SNAPSHOT_TS：成交已含在快照里，丢弃
        r.on_event(&fill_on(EX, Side::Long, 1.0, SNAPSHOT_TS));
        // local_ts > SNAPSHOT_TS：快照之后的新成交，累加
        r.on_event(&fill_on(EX, Side::Long, 4.0, SNAPSHOT_TS + 5));
        // 镜像 = 2.0(快照) + 4.0 = 6.0
        r.reconcile(EX, &[position(EX, 6.0)])
            .expect("快照已含的 Fill 不该重复累加");
        let err = r
            .reconcile(EX, &[position(EX, 7.0)])
            .expect_err("若旧 Fill 也被累加，本地会是 7.0 而这里不会报漂移");
        assert!(err.contains("本地 6"), "got: {err}");
    }

    /// 重复注册（再晋升）不得覆写镜像存量：镜像在实例撤下期间也一直在跟 Fill
    #[test]
    fn re_registration_never_overwrites_the_mirror() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 2.0)]);
        r.on_event(&fill(Side::Long, 1.0)); // 镜像 3.0
        // 再晋升：新快照读到 3.0，但镜像已 seed 过 —— 静默跳过，不覆写
        r.register(
            &[SYM.to_string()],
            &[PositionBaseline {
                position: position(EX, 3.0),
                snapshot_req_ts: 100,
            }],
        );
        r.reconcile(EX, &[position(EX, 3.0)])
            .expect("镜像应保持 3.0（基线 2.0 + Fill 1.0），与交易所一致");
    }

    /// 跟踪范围外的 symbol 不参与对账（分桶部署下账户级读数含其他桶的 symbol）
    #[test]
    fn untracked_symbols_are_ignored() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 1.0)]);
        let other_bucket = Position {
            exchange: EX,
            symbol: "ETH".to_string(),
            size: 42.0,
        };
        // 读数里混入其他桶的 symbol；本桶的 BTC 对得上，就不该有漂移
        r.reconcile(EX, &[position(EX, 1.0), other_bucket])
            .expect("其他桶的 symbol 不该触发漂移");
    }

    /// 计数按 (所, symbol) 分开：一个所的漂移不该借另一个所的次数达标
    #[test]
    fn streaks_are_per_exchange() {
        let mut r = reconciler_with(2, &[baseline_on(EX, 1.0), baseline_on(OTHER_EX, 0.0)]);

        // Binance 不一致一次
        r.reconcile(EX, &[position(EX, 9.0)]).unwrap();
        // OKX 上本地与读数都是 0 -> 一致，不该影响 Binance 的计数
        r.reconcile(OTHER_EX, &[]).unwrap();
        // Binance 第二次不一致 -> 达标
        let err = r
            .reconcile(EX, &[position(EX, 9.0)])
            .expect_err("同一个所连续两次应致命");
        assert!(err.contains("Binance"), "got: {err}");
    }

    /// 同一 symbol 跨所独立：Binance 漂了，OKX 那条腿的读数不受影响
    #[test]
    fn per_exchange_legs_are_compared_independently() {
        let mut r = reconciler_with(1, &[baseline_on(EX, 1.0), baseline_on(OTHER_EX, -1.0)]);

        // OKX 腿一致
        r.reconcile(OTHER_EX, &[position(OTHER_EX, -1.0)])
            .expect("OKX 腿对得上");
        // Binance 腿不一致
        let err = r
            .reconcile(EX, &[position(EX, 4.0)])
            .expect_err("Binance 腿应报漂移");
        assert!(err.contains("Binance"), "got: {err}");
        assert!(!err.contains("OKX"), "不该牵连对得上的 OKX 腿: {err}");
    }

    /// 镜像只吃基线与成交：对账读数本身绝不能写进镜像（否则漂移会被自我抹平）
    #[test]
    fn report_never_feeds_the_mirror() {
        let mut r = reconciler_with(2, &[baseline_on(EX, 1.0)]);
        // 第一次不一致
        r.reconcile(EX, &[position(EX, 7.0)]).unwrap();
        // 若读数写进了镜像，这次就会"一致"从而清零，永远等不到致命
        let err = r
            .reconcile(EX, &[position(EX, 7.0)])
            .expect_err("读数若写进镜像，漂移会被自我抹平");
        assert!(err.contains("本地 1"), "镜像被读数污染了: {err}");
    }
}

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

/// 连续多少次不一致才判定为真实漂移。
///
/// 不能取 1：REST 快照的时点与本地 Fill 流之间必然有间隙（成交已落交易所、Fill 稍后才到），
/// 单次不一致是正常的飞行窗口。连续多次仍不一致才说明是真漂移而非时序错位。
pub const DEFAULT_MAX_CONSECUTIVE_MISMATCHES: u32 = 3;

/// 观测层不做订单超时清理（镜像只关心持仓）
const NO_ORDER_TIMEOUT: u64 = 0;

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
    /// 容差来源：`size_step` 是该 symbol 的最小可交易增量，小于它的差值不可能是真实漂移
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// per (所, symbol) 的连续不一致次数；任意一次落回容差内即清零
    streaks: HashMap<(Exchange, Symbol), u32>,
    max_consecutive: u32,
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
            streaks: HashMap::new(),
            max_consecutive,
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
                    self.mirror.apply(event);
                    // 基线到达才让这条腿进入对账范围（见 `baselined` 字段说明）
                    self.baselined
                        .insert((position.exchange, position.symbol.clone()));
                }
                Ok(())
            }
            ExchangeEventData::Fill(fill) => {
                if self.is_tracked(&fill.symbol) {
                    self.mirror.apply(event);
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

        // 拆开字段以取得互不重叠的借用（baselined 只读、streaks 可变）
        let Self {
            mirror,
            baselined,
            symbol_metas,
            streaks,
            max_consecutive,
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
            let diff = (local_size - reported_size).abs();
            let key = (exchange, symbol.clone());

            if diff <= tolerance {
                // 一致即清零：计数器衡量的是"持续不一致"，不是历史累计
                streaks.remove(&key);
                continue;
            }

            let streak = *streaks
                .entry(key)
                .and_modify(|count| *count += 1)
                .or_insert(1);

            // 打成结构化日志，让漂移成为可观测的趋势，而不是只有停机那一刻才被看到
            tracing::warn!(
                target: "metrics",
                %exchange,
                %symbol,
                local_size = format!("{local_size:.8}"),
                reported_size = format!("{reported_size:.8}"),
                diff = format!("{diff:.8}"),
                tolerance = format!("{tolerance:.8}"),
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
            "持仓对账连续 {} 次不一致，本地「基线 + Fill」已不可信: {}",
            self.max_consecutive,
            confirmed_drift.join("; ")
        ))
    }
}

/// 容差 = 该 symbol 的最小可交易增量。
///
/// 缺 meta 说明该 symbol 未在此所上市（那么两边都是 0，容差取多少都一样），退化为浮点
/// 误差量级。下界钉在 `Position::EPSILON`：`size_step` 再小也不该小于浮点噪声。
fn tolerance_of(
    symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
    exchange: Exchange,
    symbol: &Symbol,
) -> f64 {
    symbol_metas
        .get(&(exchange, symbol.clone()))
        .map(|meta| meta.size_step)
        .unwrap_or(Position::EPSILON)
        .max(Position::EPSILON)
}

// ============================================================================
// Actor 包装
// ============================================================================

/// PositionReconcileActor 初始化参数
pub struct PositionReconcileArgs {
    /// 用于取 `size_step` 作为对账容差
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
        ev(ExchangeEventData::Fill(Fill {
            exchange: EX,
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
        assert!(err.contains("对账连续 3 次不一致"), "got: {err}");
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

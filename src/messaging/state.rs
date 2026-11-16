use crate::domain::{
    Exchange, Order, OrderId, OrderStatus, Position, Quantity, Symbol, Timestamp, BBO,
};
use crate::messaging::event::{
    OrderRequestResult, RequestOutcome, SymbolAccountData, SymbolMarketData,
};
use std::collections::HashMap;

/// 撤单发出后这么久交易所还没推终态就停机（见 [`SymbolOrders::check_stuck`]）。
///
/// 正常在秒级以内；一分钟还没等到，说明回报丢了或解析错了 —— 这张单还挂不挂着说不清，继续交易会在旁边
/// 重复挂单，故停机交人工核对。
pub const ORDER_STUCK_MS: u64 = 60_000;

/// 「下单中」这么久既没有下单请求的结论、也没有交易所回报，就按下单失败处理（见
/// [`SymbolOrders::expire_placing`]），打 error 日志、不停机（#899 用户定）。
///
/// 下单请求自带 10 秒超时；结果说不清（超时、5xx）时出口不回报结论，单留在「下单中」，这里再多等 5 秒
/// 交易所回报。之后才到的回报或受理结论会把单接回列表（见 [`SymbolOrders::revive`]），策略照常处理它。
pub const PLACE_TIMEOUT_MS: u64 = 15_000;

/// 本策略发出、还开着的一张单在本地的记录。
///
/// 订单列表只由订单这一侧的消息维护（#899 用户定）：策略发单时登记为「下单中」，下单请求的结论和交易所的
/// 订单回报推进它，交易所说它结束了（成交完 / 撤销 / 拒单）就移出。成交只进持仓（[`SymbolPositions`]），
/// 与订单列表无关 —— 所以单移出时，它的成交可能还在路上（HL 的成交和订单回报分两路推）。
///
/// 生命周期是一个状态机（[`OrderStage`]），只能由下面的转换方法推进；字段不对外可写。
#[derive(Debug, Clone)]
pub struct PendingOrder {
    /// 发出的那张单（已取整，数量与交易所看到的一致）
    pub order: Order,
    pub created_at: Timestamp,
    /// 交易所订单号：下单受理或第一条订单回报到了才有（只用于排查日志）
    exchange_order_id: Option<OrderId>,
    stage: OrderStage,
}

/// 一张开着的单走到了哪一步，每一步只带自己的数据。
///
/// 只往前走：下单中 → 挂着 → 撤单中；交易所推来终态时，从哪一步都直接移出列表。两路输入（交易所回报、
/// 本地 REST 结论）到达顺序没有保证，所以每个转换都写成与顺序无关：晚到的旧消息落在已经走过的状态上，
/// 什么也不改。
#[derive(Debug, Clone, PartialEq)]
pub enum OrderStage {
    /// 下单请求已发出，下单结论和交易所回报都还没到。此时不撤：撤单可能先于下单到交易所、报「单不存在」
    /// （#769 多挂 2 秒、白耗一次撤单）。超过 [`PLACE_TIMEOUT_MS`] 仍如此就按下单失败处理
    Placing,
    /// 交易所在撮合，没撤过
    Live,
    /// 撤过、交易所还没推终态。卡单从第一次撤起计时，中间撤单失败、重撤都不重新计时（#767）
    Cancelling {
        first_sent_at: Timestamp,
        attempt: CancelAttempt,
    },
}

/// 最近一次撤单走到哪
#[derive(Debug, Clone, PartialEq)]
pub enum CancelAttempt {
    /// 撤单已发出，等交易所推终态或撤单失败的结论；期间框架不重发
    InFlight,
    /// 撤单请求失败；单若还开着，隔多久重撤由策略定（从第一次撤起 [`ORDER_STUCK_MS`] 内没到终态，框架停机）
    Failed { at: Timestamp, reason: String },
}

/// 订单列表给策略的结论：一张单被交易所接下了，或下单失败了。
///
/// 每种结论对每张单至多一次，与它从哪条路来（下单请求的结论、交易所回报、下单超时）无关 —— 同一张单的
/// 拒单常从两条路各到一次，去重在这里做，策略不用管。随引起它的那条事件交给策略（见
/// [`crate::strategy::StrategyView::order_notices`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct OrderNotice {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub client_order_id: String,
    pub kind: OrderNoticeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OrderNoticeKind {
    /// 交易所接下了这张单：下单受理，或第一条回报说它挂着 / 成交了 / 撤了
    Accepted,
    /// 下单失败：交易所拒单（下单请求被拒，或回报说拒单 / 过期作废）、请求没发出、本地校验不过，或
    /// 下单中超过 [`PLACE_TIMEOUT_MS`]
    PlaceFailed { reason: String },
}

impl PendingOrder {
    fn new(order: Order, created_at: Timestamp) -> Self {
        Self {
            order,
            created_at,
            exchange_order_id: None,
            stage: OrderStage::Placing,
        }
    }

    pub fn stage(&self) -> &OrderStage {
        &self.stage
    }

    /// 交易所对这张单说了话（下单受理，或任何一条回报）：下单中 → 挂着。返回它是不是这一下才被接下
    fn accept(&mut self) -> bool {
        let first = self.stage == OrderStage::Placing;
        if first {
            self.stage = OrderStage::Live;
        }
        first
    }

    /// 要撤这张单：`Ok` = 该发撤单，已记为撤单在途；`Err` 带不发的原因。
    fn request_cancel(&mut self, now: Timestamp) -> Result<(), &'static str> {
        self.stage = match &self.stage {
            OrderStage::Placing => return Err("下单请求还没结论"),
            OrderStage::Cancelling {
                attempt: CancelAttempt::InFlight,
                ..
            } => return Err("上一次撤单还在等结论"),
            OrderStage::Live => OrderStage::Cancelling {
                first_sent_at: now,
                attempt: CancelAttempt::InFlight,
            },
            OrderStage::Cancelling {
                first_sent_at,
                attempt: CancelAttempt::Failed { .. },
            } => OrderStage::Cancelling {
                first_sent_at: *first_sent_at,
                attempt: CancelAttempt::InFlight,
            },
        };
        Ok(())
    }

    /// 撤单请求失败。只会落在撤单中（撤单只由 [`Self::request_cancel`] 发出，发出即进入撤单中）
    fn on_cancel_failed(&mut self, at: Timestamp, reason: &str) {
        if let OrderStage::Cancelling { attempt, .. } = &mut self.stage {
            *attempt = CancelAttempt::Failed {
                at,
                reason: reason.to_string(),
            };
        }
    }
}
/// 单个交易对的**行情**投影：各所最新盘口（其余行情策略在 on_event 里自取，框架不缓存）。
///
/// 三个投影（行情 / 持仓 / 挂单）分开的理由见 `docs/architecture.md` V1：它们的消费者
/// 互不重叠 —— 对账只要持仓，策略要行情 + 持仓 + 挂单，观测还要账户。焊在一个结构里的
/// 直接后果是"想只查持仓"做不到，只能整份复制。
#[derive(Debug, Clone, Default)]
pub struct SymbolMarket {
    /// 各所最新盘口，连同它的本地接收时刻（策略判盘口新不新鲜用，见 [`Self::bbo_received_at`]）
    bbos: HashMap<Exchange, (BBO, Timestamp)>,
}

/// 单个交易对的**持仓**投影：「投产基线 + Fill 累加」的折叠结果。
///
/// 与 seed 标记同住一处 —— "这条腿是未知还是空仓"是持仓语义的一部分，分开存会让两者失步。
#[derive(Debug, Clone, Default)]
pub struct SymbolPositions {
    pub(crate) positions: HashMap<Exchange, Position>,
    /// 各所持仓基线的**快照请求时刻**（见 [`Self::seed`]）。
    ///
    /// 兼作"该所已播种"的判据：在表内 = 基线已写入，Fill 按时刻过滤后累加；
    /// 不在表内 = 从 0 起算（模拟账户、以及未配置凭证的所）。
    pub(crate) seeded_at: HashMap<Exchange, Timestamp>,
}

/// 单个交易对的**挂单**投影：本策略发出、还开着的单（见 [`PendingOrder`]）。
///
/// 输入三种：策略发单（[`Self::add`]）、策略撤单（[`Self::mark_cancel_sent`]）、订单这一侧的消息 —— 交易所的
/// 订单回报和本地 REST 请求的结论（[`OrderRequestResult`]）。后两类走不同的路径，到达顺序没有保证，所以每条
/// 规则都写成与顺序无关的形态（见 [`OrderStage`]）。成交不是输入（只进持仓）。不在列表里的单的回报一律不重建记录，
/// 只有下单超时按失败处理过的单例外（见 [`Self::revive`]）。
#[derive(Debug, Clone, Default)]
pub struct SymbolOrders {
    /// client_order_id → 记录
    pub(crate) orders: HashMap<String, PendingOrder>,
    /// 下单中超时、已按下单失败处理的单（见 [`PLACE_TIMEOUT_MS`]）：之后若收到它的消息，接回列表
    expired: HashMap<String, PendingOrder>,
}

/// 单个交易对在所有交易所的聚合状态：三个投影的门面。
///
/// 门面保留是因为"一个 symbol 跨所的全貌"确实是策略推理的单位；但**投影之间不共享字段**，
/// 只需其中一份的消费者（如持仓账本）可以只持那一份。
#[derive(Debug, Clone)]
pub struct SymbolState {
    pub symbol: Symbol,
    market: SymbolMarket,
    position_book: SymbolPositions,
    order_book: SymbolOrders,
}

impl SymbolMarket {
    /// 获取某个交易所的 BBO
    pub fn bbo(&self, exchange: Exchange) -> Option<&BBO> {
        self.bbos.get(&exchange).map(|(bbo, _)| bbo)
    }

    /// 某所最新盘口的本地接收时刻（与事件的 `local_ts` 同一时钟；没收到过为 `None`）
    pub fn bbo_received_at(&self, exchange: Exchange) -> Option<Timestamp> {
        self.bbos.get(&exchange).map(|(_, at)| *at)
    }

    /// 行情类事件的状态转移（只缓存盘口，其余策略在 on_event 里自取）
    pub fn apply(&mut self, data: &SymbolMarketData, local_ts: Timestamp) {
        match data {
            SymbolMarketData::BBO(bbo) => {
                self.bbos.insert(bbo.exchange, (bbo.clone(), local_ts));
            }
            SymbolMarketData::FundingRate(_)
            | SymbolMarketData::MarkPrice(_)
            | SymbolMarketData::IndexPrice(_)
            | SymbolMarketData::MarketTrade(_)
            | SymbolMarketData::Candle(_)
            | SymbolMarketData::HistoryCandles(_) => {}
        }
    }
}

impl SymbolPositions {
    /// 累加一笔成交。**持仓维护的增量输入只有这一条路。**
    ///
    /// Fill 涵盖策略单与强平/ADL 等不在跟踪表里的成交 —— 都以 fill 形式经私有成交流到达，
    /// 走同一路径（模型说明见 [`Self::seed`]）。
    ///
    /// # 防双计：已 seed 的所按快照请求时刻过滤
    ///
    /// 在快照请求时刻**之前**送达的 Fill，其成交必已含在基线快照里，再累加即双计。
    /// 这条规则对全部基线消费者（executor / 持仓账本 / 观测镜像）**必须是同一份** ——
    /// 所以它安在这里，与 `seeded_at` 同一个结构，而不是各消费者自己写一遍。
    ///
    /// 持仓维护的全部内容就是**累加带符号数量**：策略决策只看数量（还能加多少、要平多少、
    /// 净敞口是否为零）。均价/盈亏不在域模型里 —— 记账需求由模拟柜台的账本
    /// （`sim::BookPosition`）完成，
    /// 见 [`crate::domain::Position`] 的文档。
    pub fn apply_fill(&mut self, symbol: &Symbol, fill: &crate::domain::Fill, local_ts: Timestamp) {
        if let Some(&seeded_ts) = self.seeded_at.get(&fill.exchange) {
            if local_ts <= seeded_ts {
                tracing::info!(
                    %symbol,
                    exchange = %fill.exchange,
                    fill_size = %fill.size,
                    fill_local_ts = local_ts,
                    snapshot_req_ts = seeded_ts,
                    "Fill 早于基线快照请求时刻送达（已含在快照里），丢弃避免双计"
                );
                return;
            }
        }
        let pos = self
            .positions
            .entry(fill.exchange)
            .or_insert_with(|| Position::empty(fill.exchange, symbol.clone()));
        pos.add_fill(fill.side, fill.size);
        tracing::info!(
            %symbol,
            exchange = %fill.exchange,
            side = ?fill.side,
            fill_size = %fill.size,
            fill_price = %fill.price,
            reason = ?fill.reason,
            new_position_size = %pos.size,
            "Updated position on fill"
        );
    }

    /// 写入持仓**基线**（投产握手的载荷，不是事件 —— 见 [`crate::messaging::PositionBaseline`]）。
    ///
    /// 持仓维护模型：**一次性基线 + 之后全程由 Fill 累加**。基线只能写一次，因为快照与
    /// 实时 Fill 流之间存在竞态：快照可能已含某笔成交，其 Fill 稍后才到 —— 若覆写后再叠加
    /// 就会重复计算。为此：
    ///
    /// - `snapshot_req_ts` 是 REST 快照的**请求**时刻。在此之前送达（`local_ts <=`）的
    ///   Fill 必已含在快照里，seed 之后到达时被 [`Self::apply_fill`] 丢弃 ——
    ///   这条过滤规则对所有基线消费者（executor、对账镜像、观测镜像）同一份。
    /// - 只投产一次，每所只 seed 一次，且在任何 Fill 进来之前（executor 出生即带基线；镜像
    ///   注册与基线原子到达，未注册 symbol 的 Fill 不进镜像）。
    ///
    /// # 如实声明的残余窗口（没有交易所侧序号无法根除）
    ///
    /// - 成交发生在快照**之前**（已含在快照）、Fill 在快照请求之后才送达
    ///   （`local_ts > snapshot_req_ts`）—— 会双计，窗口 = 交易所撮合到 Fill 送达的时延。
    /// - 快照之后、注册完成之前的外部成交：executor 与镜像可能分歧，由持仓账本检出并停机（#694）。
    ///
    /// 都只可能来自投产瞬间不在跟踪表里的成交（强平/ADL 等），与旧的总线投递方案等宽。
    pub fn seed(&mut self, symbol: &Symbol, position: &Position, snapshot_req_ts: Timestamp) {
        tracing::info!(
            %symbol,
            exchange = %position.exchange,
            size = %position.size,
            snapshot_req_ts,
            "Position baseline seeded"
        );
        self.seeded_at.insert(position.exchange, snapshot_req_ts);
        self.positions.insert(position.exchange, position.clone());
    }

    /// 该所的持仓基线是否已写入（写入后该腿才可参与对账，见 `PositionLedger`）
    pub fn is_seeded(&self, exchange: Exchange) -> bool {
        self.seeded_at.contains_key(&exchange)
    }

    /// 基线已写入的各所持仓（**含 `size == 0` 的空仓腿**）。
    ///
    /// 未 seed 的腿被排除：那时本地持仓是「未知」而不是 0。这条区分对两个消费者都是必须的 ——
    /// 对账不能拿未知去比（必然误判漂移），对外查询也不能把未知当空仓报出去
    /// （下游会据此算出假的对冲完整性）。规则的家安在这里，与 seed 标记同一处。
    pub fn seeded_positions(&self) -> impl Iterator<Item = &Position> {
        self.positions
            .values()
            .filter(|p| self.is_seeded(p.exchange))
    }

    /// 获取某个交易所的仓位
    pub fn position(&self, exchange: Exchange) -> Option<&Position> {
        self.positions.get(&exchange)
    }

    /// 获取某个交易所的仓位大小。**无记录与空仓都返回 0**。
    ///
    /// 压平在"算下单量"这个用途上永远安全（理由见
    /// [`crate::strategy::SymbolView::position_size`]）；要拿"有没有这条记录"当交易前置
    /// 条件，用 [`Self::position`] —— 它保留了 `Option`，那里也写清了 `None` 的三种来路。
    ///
    /// 此前这里的注释写的是"无仓位记录等价于空仓，这是正确的业务语义"。那句话在下单量
    /// 场景下成立，但它没有限定用途，读起来像是断言两者在任何场景下都等价 —— 而模拟策略
    /// 首笔成交前、以及无凭证的所，恰恰是需要分辨的。
    pub fn position_size(&self, exchange: Exchange) -> Quantity {
        self.positions
            .get(&exchange)
            .map_or(Quantity::ZERO, |p| p.size)
    }

    /// 获取多空仓位大小
    ///
    /// 返回 (多头总量, 空头总量):
    /// - 多头总量: 所有正向持仓之和（正数）
    /// - 空头总量: 所有负向持仓之和（负数）
    pub fn position_sizes(&self) -> (Quantity, Quantity) {
        let mut long_size = Quantity::ZERO;
        let mut short_size = Quantity::ZERO;

        for pos in self.positions.values() {
            if pos.size > Quantity::ZERO {
                long_size += pos.size;
            } else if pos.size < Quantity::ZERO {
                short_size += pos.size;
            }
        }

        (long_size, short_size)
    }
}

impl SymbolOrders {
    /// 登记策略刚发出的一张单（client_order_id 已分配）：下单中
    pub fn add(&mut self, order: Order, created_at: Timestamp) {
        self.orders.insert(
            order.client_order_id.clone(),
            PendingOrder::new(order, created_at),
        );
    }

    /// 策略要撤这张单：记下「撤单在途」，返回 `Ok`（带这张单所在的所）表示该发。
    ///
    /// 不该发的（`Err` 带原因）：不在列表里（已结束）、下单请求还没结论、上一次撤单还在等结论（见
    /// [`OrderStage`]）。下单没结论时不撤：受理的结论（或交易所回报）一到策略就会被调用、可以再撤，只晚一个往返。
    pub fn mark_cancel_sent(
        &mut self,
        client_order_id: &str,
        now: Timestamp,
    ) -> Result<Exchange, &'static str> {
        let p = self
            .orders
            .get_mut(client_order_id)
            .ok_or("不在订单列表里")?;
        p.request_cancel(now)?;
        Ok(p.order.exchange)
    }

    /// 交易所的订单回报：非终态把下单中推成挂着，终态移出（拒单即下单失败）
    fn apply_update(
        &mut self,
        symbol: &Symbol,
        update: &crate::domain::OrderUpdate,
    ) -> Option<OrderNotice> {
        let cid = update.client_order_id.as_deref()?; // 没有订单号：列表对不上它
        if !update.status.is_terminal() {
            self.revive(symbol, cid);
        } else if self.expired.remove(cid).is_some() {
            // 超时时已按下单失败报过策略；它已结束，不再接回（有成交的照常进持仓）
            tracing::error!(%symbol, exchange = %update.exchange, %cid, status = ?update.status, "已按下单失败处理的单收到终态回报");
            return None;
        }
        let Some(p) = self.orders.get_mut(cid) else {
            if !update.status.is_terminal() {
                // 还开着、本地却没有（启动时已撤净遗留挂单，正常不会出现）。不接管：它的成交照常进持仓，
                // 由策略按普通不平衡处理（见 docs/architecture.md「账户独占」）
                tracing::warn!(%symbol, exchange = %update.exchange, %cid, status = ?update.status, "挂单不在订单列表里，不接管");
            }
            return None;
        };
        p.exchange_order_id
            .get_or_insert_with(|| update.order_id.clone());
        let accepted = p.accept();
        let kind = match &update.status {
            OrderStatus::Rejected { reason } => Some(OrderNoticeKind::PlaceFailed {
                reason: reason.clone(),
            }),
            OrderStatus::Pending
            | OrderStatus::PartiallyFilled
            | OrderStatus::Filled
            | OrderStatus::Cancelled => accepted.then_some(OrderNoticeKind::Accepted),
        };
        if update.status.is_terminal() {
            let p = self.orders.remove(cid).expect("刚查过");
            tracing::info!(%symbol, exchange = %p.order.exchange, %cid, status = ?update.status, "订单结束，移出订单列表");
        }
        kind.map(|kind| notice(symbol, update.exchange, cid, kind))
    }

    /// 本地 REST 请求的结论。单已不在列表里（交易所回报先到、已结束）就不理。
    fn apply_request(
        &mut self,
        symbol: &Symbol,
        r: &OrderRequestResult,
        local_ts: Timestamp,
    ) -> Option<OrderNotice> {
        let cid = r.client_order_id.as_str();
        match &r.outcome {
            RequestOutcome::Placed { order_id } => {
                self.revive(symbol, cid);
                let p = self.orders.get_mut(cid)?;
                p.exchange_order_id.get_or_insert_with(|| order_id.clone());
                p.accept()
                    .then(|| notice(symbol, r.exchange, cid, OrderNoticeKind::Accepted))
            }
            RequestOutcome::PlaceRejected { reason } => {
                if self.expired.remove(cid).is_some() {
                    // 超时时已按下单失败报过策略；现在确认确实没挂上
                    tracing::info!(%symbol, exchange = %r.exchange, %cid, %reason, "超时的单确认下单被拒");
                    return None;
                }
                let p = self.orders.get(cid)?;
                if p.stage != OrderStage::Placing {
                    tracing::error!(%symbol, exchange = %r.exchange, %cid, %reason, stage = ?p.stage,
                        "下单报被拒，但交易所已回报过这张单：以交易所回报为准，不移出");
                    return None;
                }
                tracing::info!(%symbol, exchange = %r.exchange, %cid, %reason, "下单被拒，移出订单列表");
                self.orders.remove(cid);
                Some(notice(
                    symbol,
                    r.exchange,
                    cid,
                    OrderNoticeKind::PlaceFailed {
                        reason: reason.clone(),
                    },
                ))
            }
            RequestOutcome::CancelFailed { reason } => {
                if let Some(p) = self.orders.get_mut(cid) {
                    p.on_cancel_failed(local_ts, reason);
                }
                None
            }
        }
    }

    /// 超时按失败处理过的单又有了消息（回报或受理）：接回列表，当它还在下单中，随后的转换照常走
    fn revive(&mut self, symbol: &Symbol, cid: &str) {
        if let Some(p) = self.expired.remove(cid) {
            tracing::error!(%symbol, exchange = %p.order.exchange, %cid, "已按下单失败处理的单又收到了交易所的消息，接回订单列表");
            self.orders.insert(cid.to_string(), p);
        }
    }

    /// 下单中超过 [`PLACE_TIMEOUT_MS`] 的单按下单失败处理：移出列表（记在 `expired`，之后有消息再接回）
    fn expire_placing(&mut self, symbol: &Symbol, now: Timestamp) -> Vec<OrderNotice> {
        let stale: Vec<String> = self
            .orders
            .iter()
            .filter(|(_, p)| {
                p.stage == OrderStage::Placing
                    && now.saturating_sub(p.created_at) > PLACE_TIMEOUT_MS
            })
            .map(|(cid, _)| cid.clone())
            .collect();
        stale
            .into_iter()
            .map(|cid| {
                let p = self.orders.remove(&cid).expect("刚查过");
                let reason = format!(
                    "下单 {}ms 没有结论也没有交易所回报，按下单失败处理",
                    now.saturating_sub(p.created_at)
                );
                tracing::error!(%symbol, exchange = %p.order.exchange, %cid, order = ?p.order, %reason);
                let exchange = p.order.exchange;
                self.expired.insert(cid.clone(), p);
                notice(
                    symbol,
                    exchange,
                    &cid,
                    OrderNoticeKind::PlaceFailed { reason },
                )
            })
            .collect()
    }

    /// 有单撤单发出后超过 [`ORDER_STUCK_MS`] 交易所还没推终态就报错（调用方停机）
    fn check_stuck(&self, symbol: &Symbol, now: Timestamp) -> Result<(), String> {
        for (cid, p) in &self.orders {
            if let OrderStage::Cancelling { first_sent_at, .. } = &p.stage {
                let waited = now.saturating_sub(*first_sent_at);
                if waited > ORDER_STUCK_MS {
                    return Err(format!(
                        "{symbol} {} 订单 {cid}（交易所号 {:?}）撤单发出后交易所 {waited}ms 没推终态：{:?}。\
                         说不清这张单还挂不挂着，停机交人工核对挂单与持仓",
                        p.order.exchange, p.exchange_order_id, p.stage
                    ));
                }
            }
        }
        Ok(())
    }

    /// 是否还有开着的单
    pub fn has_pending(&self) -> bool {
        !self.orders.is_empty()
    }

    /// 全部开着的单
    pub fn iter(&self) -> impl Iterator<Item = &PendingOrder> {
        self.orders.values()
    }
}

fn notice(symbol: &Symbol, exchange: Exchange, cid: &str, kind: OrderNoticeKind) -> OrderNotice {
    OrderNotice {
        exchange,
        symbol: symbol.clone(),
        client_order_id: cid.to_string(),
        kind,
    }
}

impl SymbolState {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            market: SymbolMarket::default(),
            position_book: SymbolPositions::default(),
            order_book: SymbolOrders::default(),
        }
    }

    /// 行情事件的状态转移。调用方（`StateManager`）已按 symbol 路由。
    ///
    /// 只收 [`SymbolMarketData`]：所级与全局行情在类型上就到不了这里。
    pub fn apply_market(&mut self, data: &SymbolMarketData, local_ts: Timestamp) {
        debug_assert_eq!(
            data.symbol(),
            &self.symbol,
            "行情被路由到了别的 symbol 状态"
        );
        self.market.apply(data, local_ts);
    }

    /// 币级账户事件的状态转移：订单回报与下单/撤单请求的结论只进订单列表，成交只进持仓。
    /// 返回订单列表因此得出的结论（见 [`OrderNotice`]）。调用方已按 symbol 路由。
    ///
    /// 只收 [`SymbolAccountData`]：所级账户事件在类型上就到不了这里。
    pub fn apply_account(
        &mut self,
        data: &SymbolAccountData,
        local_ts: Timestamp,
    ) -> Option<OrderNotice> {
        debug_assert_eq!(
            data.symbol(),
            &self.symbol,
            "账户事件被路由到了别的 symbol 状态"
        );
        match data {
            SymbolAccountData::OrderUpdate(update) => {
                tracing::info!(
                    symbol = %self.symbol,
                    exchange = %update.exchange,
                    order_id = %update.order_id,
                    client_order_id = ?update.client_order_id,
                    status = ?update.status,
                    filled_quantity = %update.filled_quantity,
                    "Updating order status"
                );
                self.order_book.apply_update(&self.symbol, update)
            }
            SymbolAccountData::OrderRequest(r) => {
                self.order_book.apply_request(&self.symbol, r, local_ts)
            }
            SymbolAccountData::Fill(fill) => {
                // 不相交字段借用：symbol 与 position_book 是两个字段，无需为每条 Fill 克隆
                self.position_book.apply_fill(&self.symbol, fill, local_ts);
                None
            }
            SymbolAccountData::FundingFee(_) => {
                // 资费事件不修改本地 symbol 状态，下游策略自行去重与统计
                None
            }
        }
    }

    // ==================== 投影访问 ====================

    /// 行情投影（只读）
    pub fn market(&self) -> &SymbolMarket {
        &self.market
    }

    /// 持仓投影（只读）
    pub fn position_book(&self) -> &SymbolPositions {
        &self.position_book
    }

    /// 挂单投影（只读）
    pub fn order_book(&self) -> &SymbolOrders {
        &self.order_book
    }

    // ==================== 写入口 ====================
    // 读一律经上面三个投影；写要带上本 symbol（日志与校验用），且投影只读外放，故写入口留在这里

    pub fn seed_position(&mut self, position: &Position, snapshot_req_ts: Timestamp) {
        self.position_book
            .seed(&self.symbol, position, snapshot_req_ts)
    }

    pub fn add_pending_order(&mut self, order: Order, created_at: Timestamp) {
        self.order_book.add(order, created_at);
    }

    /// 策略要撤这张单，见 [`SymbolOrders::mark_cancel_sent`]
    pub fn mark_cancel_sent(
        &mut self,
        client_order_id: &str,
        now: Timestamp,
    ) -> Result<Exchange, &'static str> {
        self.order_book.mark_cancel_sent(client_order_id, now)
    }

    /// 时钟拍：撤单卡太久报错（调用方停机，见 [`SymbolOrders::check_stuck`]）；下单中超时的单按下单失败处理
    /// （见 [`SymbolOrders::expire_placing`]）
    pub fn on_clock(&mut self, now: Timestamp) -> Result<Vec<OrderNotice>, String> {
        self.order_book.check_stuck(&self.symbol, now)?;
        Ok(self.order_book.expire_placing(&self.symbol, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Side;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    const SYMBOL: &str = "BTC";

    // ===== 持仓维护模型：基线只来一次 + Fill 累加 + 对账读数不写入 =====

    const EX: Exchange = Exchange::Binance;

    /// 一条币级账户事件及其送达时刻
    type Ev = (SymbolAccountData, Timestamp);

    fn apply(state: &mut SymbolState, (data, local_ts): &Ev) {
        state.apply_account(data, *local_ts);
    }

    fn position(size: Decimal) -> Position {
        Position {
            exchange: EX,
            symbol: SYMBOL.to_string(),
            size,
        }
    }

    fn fill(side: Side, size: Decimal) -> Ev {
        fill_at(EX, side, size, 1)
    }

    fn fill_at(exchange: Exchange, side: Side, size: Decimal, local_ts: u64) -> Ev {
        (
            SymbolAccountData::Fill(crate::domain::Fill {
                exchange,
                symbol: SYMBOL.to_string(),
                side,
                price: dec!(100.0),
                size,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: local_ts,
                fee: None,
                reason: crate::domain::FillReason::Normal,
            }),
            local_ts,
        )
    }

    /// 基线写入一次，之后**只**由 Fill 累加。
    #[test]
    fn baseline_initializes_then_fills_accumulate() {
        let mut state = SymbolState::new(SYMBOL.to_string());

        state.seed_position(&position(dec!(2.0)), 0);
        assert_eq!(state.position_book().position_size(EX), dec!(2.0));

        apply(&mut state, &fill(Side::Long, dec!(1.0)));
        assert_eq!(state.position_book().position_size(EX), dec!(3.0));

        apply(&mut state, &fill(Side::Short, dec!(0.5)));
        assert_eq!(state.position_book().position_size(EX), dec!(2.5));
    }

    /// **防双计**：seed 之后到达、但送达时刻早于快照请求时刻的 Fill 已含在快照里，丢弃；
    /// 之后的照常累加。未 seed 的所不受过滤影响。
    #[test]
    fn fills_covered_by_snapshot_are_dropped() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 快照请求时刻 t=10
        state.seed_position(&position(dec!(2.0)), 10);

        // t=1 送达（fill() 的 local_ts=1 <= 10）：已含在快照里，丢弃
        apply(&mut state, &fill(Side::Long, dec!(1.0)));
        assert_eq!(
            state.position_book().position_size(EX),
            dec!(2.0),
            "快照已含的 Fill 不得再累加"
        );

        // t=11 送达：快照之后的新成交，累加
        apply(&mut state, &fill_at(EX, Side::Long, dec!(1.0), 11));
        assert_eq!(state.position_book().position_size(EX), dec!(3.0));
    }

    /// **seed 过滤按所隔离**：只 seed 了一个所时，另一个所的早时刻 Fill 不得被误过滤
    /// （guard 按 `fill.exchange` 查表，不是按 symbol 全局生效）
    #[test]
    fn seed_filter_is_scoped_to_its_exchange() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 只 seed Binance（快照请求时刻 t=10）
        state.seed_position(&position(dec!(2.0)), 10);

        // OKX 未 seed：t=1 送达的 Fill 照常从 0 累加，不受 Binance 的过滤时刻影响
        apply(
            &mut state,
            &fill_at(Exchange::OKX, Side::Long, dec!(1.5), 1),
        );
        assert_eq!(
            state.position_book().position_size(Exchange::OKX),
            dec!(1.5),
            "未 seed 的所不得被误过滤"
        );
        assert_eq!(
            state.position_book().position_size(EX),
            dec!(2.0),
            "Binance 腿不受影响"
        );
    }

    // ===== 订单列表：只由订单回报与请求结论维护、与到达顺序无关、结论每张单只给一次 =====

    const CID: &str = "cid-1";

    fn tracked_order(qty: Decimal) -> Order {
        Order {
            exchange: EX,
            symbol: SYMBOL.to_string(),
            side: Side::Long,
            order_type: crate::domain::OrderType::Limit {
                price: dec!(100.0),
                tif: crate::domain::TimeInForce::GTC,
            },
            quantity: qty,
            reduce_only: false,
            client_order_id: CID.to_string(),
        }
    }

    /// 登记一张 1.0 的单（t=0）
    fn with_order() -> SymbolState {
        let mut state = SymbolState::new(SYMBOL.to_string());
        state.add_pending_order(tracked_order(dec!(1.0)), 0);
        state
    }

    fn update(cid: &str, status: OrderStatus, filled: Decimal, ts: u64) -> Ev {
        (
            SymbolAccountData::OrderUpdate(crate::domain::OrderUpdate {
                order_id: "ex-1".to_string(),
                client_order_id: Some(cid.to_string()),
                exchange: EX,
                symbol: SYMBOL.to_string(),
                side: Side::Long,
                status,
                quantity: dec!(1.0),
                filled_quantity: filled,
            }),
            ts,
        )
    }

    fn own_fill(size: Decimal, ts: u64) -> Ev {
        let (mut data, ts) = fill_at(EX, Side::Long, size, ts);
        if let SymbolAccountData::Fill(f) = &mut data {
            f.client_order_id = Some(CID.to_string());
        }
        (data, ts)
    }

    fn request(outcome: RequestOutcome, ts: u64) -> Ev {
        (
            SymbolAccountData::OrderRequest(OrderRequestResult {
                exchange: EX,
                symbol: SYMBOL.to_string(),
                client_order_id: CID.to_string(),
                outcome,
            }),
            ts,
        )
    }

    fn placed(ts: u64) -> Ev {
        request(
            RequestOutcome::Placed {
                order_id: "ex-1".into(),
            },
            ts,
        )
    }

    fn rejected(ts: u64) -> Ev {
        request(
            RequestOutcome::PlaceRejected {
                reason: "margin".into(),
            },
            ts,
        )
    }

    fn the_order(state: &SymbolState) -> Option<&PendingOrder> {
        state
            .order_book()
            .iter()
            .find(|p| p.order.client_order_id == CID)
    }

    /// 喂一条账户事件，返回它让订单列表得出的结论种类
    fn feed(state: &mut SymbolState, (data, ts): &Ev) -> Option<OrderNoticeKind> {
        state.apply_account(data, *ts).map(|n| {
            assert_eq!(
                (n.exchange, n.symbol.as_str(), n.client_order_id.as_str()),
                (EX, SYMBOL, CID)
            );
            n.kind
        })
    }

    fn failed(reason: &str) -> Option<OrderNoticeKind> {
        Some(OrderNoticeKind::PlaceFailed {
            reason: reason.into(),
        })
    }

    /// **#899**：成交只进持仓，订单列表不等成交。终态先于成交到达（HL 两路推送）时单照样立刻移出
    #[test]
    fn terminal_report_removes_the_order_without_waiting_for_fills() {
        let mut state = with_order();
        assert_eq!(
            feed(&mut state, &update(CID, OrderStatus::Filled, dec!(1.0), 1)),
            Some(OrderNoticeKind::Accepted),
            "第一条回报即被接下"
        );
        assert!(!state.order_book().has_pending(), "终态即移出");
        assert_eq!(state.position_book().position_size(EX), dec!(0.0));
        assert_eq!(feed(&mut state, &own_fill(dec!(1.0), 2)), None);
        assert_eq!(state.position_book().position_size(EX), dec!(1.0));
    }

    /// 成交不推进订单：成交加满了单量，终态没到，单还开着、照常可撤（撤单失败由策略重撤）
    #[test]
    fn fills_do_not_touch_the_order_list() {
        let mut state = with_order();
        feed(&mut state, &placed(1));
        feed(&mut state, &own_fill(dec!(1.0), 2));
        assert_eq!(the_order(&state).unwrap().stage(), &OrderStage::Live);
        assert!(state.mark_cancel_sent(CID, 3).is_ok());
    }

    /// 下单受理：回填订单号、下单中 → 挂着、报「被接下」；之后的回报不再报
    #[test]
    fn accepted_is_noticed_once() {
        let mut state = with_order();
        assert_eq!(
            feed(&mut state, &placed(1)),
            Some(OrderNoticeKind::Accepted)
        );
        let p = the_order(&state).unwrap();
        assert_eq!(
            (p.stage(), p.exchange_order_id.as_deref()),
            (&OrderStage::Live, Some("ex-1"))
        );
        assert_eq!(
            feed(&mut state, &update(CID, OrderStatus::Pending, dec!(0), 2)),
            None
        );
        assert_eq!(
            feed(
                &mut state,
                &update(CID, OrderStatus::PartiallyFilled, dec!(0.3), 3)
            ),
            None
        );
        assert_eq!(
            feed(
                &mut state,
                &update(CID, OrderStatus::Cancelled, dec!(0.3), 4)
            ),
            None
        );
        assert!(!state.order_book().has_pending());
    }

    /// 同一张单的拒单从两条路各到一次（下单请求被拒 + 交易所回报拒单，先后都可能），下单失败只报一次
    #[test]
    fn a_rejection_arriving_twice_is_noticed_once() {
        // 请求结论先到
        let mut state = with_order();
        assert_eq!(feed(&mut state, &rejected(1)), failed("margin"));
        assert!(!state.order_book().has_pending());
        let rejected_update = update(
            CID,
            OrderStatus::Rejected {
                reason: "ws".into(),
            },
            dec!(0),
            2,
        );
        assert_eq!(feed(&mut state, &rejected_update), None);

        // 交易所回报先到：以交易所为准，迟到的请求结论不理
        let mut state = with_order();
        assert_eq!(feed(&mut state, &rejected_update), failed("ws"));
        assert_eq!(feed(&mut state, &rejected(3)), None);
    }

    /// 拒单结论晚于交易所的非终态回报到：以交易所为准，单不移出、不报失败（否则策略会在旁边再挂一张）
    #[test]
    fn rejection_after_exchange_update_keeps_the_order() {
        let mut state = with_order();
        feed(&mut state, &update(CID, OrderStatus::Pending, dec!(0.0), 1));
        assert_eq!(feed(&mut state, &rejected(2)), None);
        assert_eq!(the_order(&state).unwrap().stage(), &OrderStage::Live);
    }

    /// 挂上以后才被作废（币安 post-only 受理后 EXPIRED）：先报被接下、再报下单失败，各一次
    #[test]
    fn rejection_after_acceptance_is_a_place_failure() {
        let mut state = with_order();
        assert_eq!(
            feed(&mut state, &placed(1)),
            Some(OrderNoticeKind::Accepted)
        );
        let expired = update(
            CID,
            OrderStatus::Rejected {
                reason: "EXPIRED".into(),
            },
            dec!(0),
            2,
        );
        assert_eq!(feed(&mut state, &expired), failed("EXPIRED"));
        assert!(!state.order_book().has_pending());
    }

    /// **#899 用户定**：下单中超过 [`PLACE_TIMEOUT_MS`] 没有任何消息 → 按下单失败处理（报一次、不报错停机）；
    /// 之后收到它的非终态回报就接回列表（策略照常撤），终态回报不接回、不再报
    #[test]
    fn placing_too_long_is_a_place_failure_and_late_reports_revive_it() {
        let mut state = with_order();
        assert_eq!(state.on_clock(PLACE_TIMEOUT_MS), Ok(vec![]));
        let notices = state.on_clock(PLACE_TIMEOUT_MS + 1).expect("超时不停机");
        assert!(
            matches!(
                notices.as_slice(),
                [OrderNotice {
                    kind: OrderNoticeKind::PlaceFailed { .. },
                    ..
                }]
            ),
            "{notices:?}"
        );
        assert!(!state.order_book().has_pending());
        assert_eq!(state.on_clock(PLACE_TIMEOUT_MS + 2), Ok(vec![]), "只报一次");

        // 迟到的受理：接回列表、挂着，报被接下（策略据此清退避、按需撤单）
        let mut revived = state.clone();
        assert_eq!(
            feed(&mut revived, &placed(20_000)),
            Some(OrderNoticeKind::Accepted)
        );
        assert_eq!(the_order(&revived).unwrap().stage(), &OrderStage::Live);
        assert!(revived.mark_cancel_sent(CID, 20_001).is_ok());

        // 迟到的非终态回报同样接回
        let mut revived = state.clone();
        feed(
            &mut revived,
            &update(CID, OrderStatus::Pending, dec!(0), 20_000),
        );
        assert!(the_order(&revived).is_some());

        // 迟到的终态回报 / 拒单结论：不接回、不再报
        let mut ended = state.clone();
        assert_eq!(
            feed(
                &mut ended,
                &update(CID, OrderStatus::Filled, dec!(1.0), 20_000)
            ),
            None
        );
        assert!(!ended.order_book().has_pending());
        let mut ended = state;
        assert_eq!(feed(&mut ended, &rejected(20_000)), None);
        assert_eq!(
            feed(&mut ended, &placed(20_001)),
            None,
            "确认被拒后不再接回"
        );
    }

    /// 撤单失败：记在单上、单还开着，可以再撤
    #[test]
    fn cancel_failure_is_recorded_on_the_order() {
        let mut state = with_order();
        feed(&mut state, &placed(1));
        state.mark_cancel_sent(CID, 2).unwrap();
        assert_eq!(
            state.mark_cancel_sent(CID, 3),
            Err("上一次撤单还在等结论"),
            "撤单在途不重发"
        );
        assert_eq!(
            feed(
                &mut state,
                &request(
                    RequestOutcome::CancelFailed {
                        reason: "busy".into(),
                    },
                    4,
                )
            ),
            None
        );
        assert_eq!(
            the_order(&state).unwrap().stage(),
            &OrderStage::Cancelling {
                first_sent_at: 2,
                attempt: CancelAttempt::Failed {
                    at: 4,
                    reason: "busy".into()
                }
            }
        );
        assert!(state.mark_cancel_sent(CID, 5).is_ok(), "失败后可以再撤");
    }

    /// 下单没结论的单不发撤单（撤单会先于下单到交易所）；不在列表里的也不发
    #[test]
    fn no_cancel_for_placing_or_unknown_orders() {
        let mut state = with_order();
        assert_eq!(state.mark_cancel_sent(CID, 1), Err("下单请求还没结论"));
        assert_eq!(
            the_order(&state).unwrap().stage(),
            &OrderStage::Placing,
            "没记成已撤，受理后还能撤"
        );
        assert_eq!(state.mark_cancel_sent("other", 2), Err("不在订单列表里"));
    }

    /// 状态机的每条转换：从每个状态出发，喂每种输入，看落到哪个状态、撤单该不该发。
    /// 旧消息落在已经走过的状态上不改任何东西（与到达顺序无关）。
    #[test]
    fn every_stage_transition() {
        use CancelAttempt::{Failed, InFlight};
        use OrderStage::{Cancelling, Live, Placing};
        let failed = || Failed {
            at: 7,
            reason: "x".into(),
        };
        let at = |stage: OrderStage| {
            let mut p = PendingOrder::new(tracked_order(dec!(1.0)), 0);
            p.stage = stage;
            p
        };
        let all = || {
            vec![
                Placing,
                Live,
                Cancelling {
                    first_sent_at: 3,
                    attempt: InFlight,
                },
                Cancelling {
                    first_sent_at: 3,
                    attempt: failed(),
                },
            ]
        };

        // 交易所说了话：只把下单中推成挂着，且只在这一下报「刚被接下」
        for from in all() {
            let mut p = at(from.clone());
            assert_eq!(p.accept(), from == Placing, "accept from {from:?}");
            assert_eq!(
                p.stage,
                if from == Placing { Live } else { from.clone() },
                "accept from {from:?}"
            );
        }
        // 撤单失败：只改撤单中的这次尝试，第一次撤的时刻不变
        for from in all() {
            let mut p = at(from.clone());
            p.on_cancel_failed(7, "x");
            let want = if matches!(from, Cancelling { .. }) {
                Cancelling {
                    first_sent_at: 3,
                    attempt: failed(),
                }
            } else {
                from.clone()
            };
            assert_eq!(p.stage, want, "撤单失败 from {from:?}");
        }
        // 要撤：只有挂着、上次撤单失败的能发；重撤保留第一次撤的时刻
        let cases: Vec<(OrderStage, Result<OrderStage, &str>)> = vec![
            (Placing, Err("下单请求还没结论")),
            (
                Live,
                Ok(Cancelling {
                    first_sent_at: 50,
                    attempt: InFlight,
                }),
            ),
            (
                Cancelling {
                    first_sent_at: 3,
                    attempt: InFlight,
                },
                Err("上一次撤单还在等结论"),
            ),
            (
                Cancelling {
                    first_sent_at: 3,
                    attempt: failed(),
                },
                Ok(Cancelling {
                    first_sent_at: 3,
                    attempt: InFlight,
                }),
            ),
        ];
        for (from, want) in cases {
            let mut p = at(from.clone());
            match want {
                Ok(to) => {
                    assert_eq!(p.request_cancel(50), Ok(()), "撤单 from {from:?}");
                    assert_eq!(p.stage, to);
                }
                Err(why) => {
                    assert_eq!(p.request_cancel(50), Err(why), "撤单 from {from:?}");
                    assert_eq!(p.stage, from, "不发就不改状态");
                }
            }
        }
    }

    /// 不在列表里的单的回报不重建记录
    #[test]
    fn updates_of_untracked_orders_never_create_records() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        for cid in ["own-1", "own-2"] {
            apply(&mut state, &update(cid, OrderStatus::Pending, dec!(0.0), 1));
            apply(
                &mut state,
                &update(cid, OrderStatus::PartiallyFilled, dec!(0.5), 2),
            );
        }
        assert!(!state.order_book().has_pending());
    }

    /// 撤单发出后交易所超过 [`ORDER_STUCK_MS`] 没推终态就报错；挂着不动不算；从第一次撤起算，中间撤单失败、
    /// 重撤不重新计时
    #[test]
    fn cancel_unanswered_too_long_is_an_error() {
        let late = ORDER_STUCK_MS + 1;
        // 挂着：多久都不算
        let mut state = with_order();
        feed(&mut state, &update(CID, OrderStatus::Pending, dec!(0.0), 1));
        assert!(state.on_clock(10 * late).is_ok());

        state.mark_cancel_sent(CID, 100).unwrap();
        assert!(state.on_clock(100 + ORDER_STUCK_MS).is_ok());
        assert!(state.on_clock(100 + late).is_err());
        // 撤单一直失败、反复重撤：仍从第一次撤起算（此前撤单失败后不算在等，该币会一直卡在只撤不挂）
        feed(
            &mut state,
            &request(RequestOutcome::CancelFailed { reason: "x".into() }, 200),
        );
        assert!(state.on_clock(100 + late).is_err());
        state.mark_cancel_sent(CID, 50_000).unwrap();
        feed(
            &mut state,
            &request(RequestOutcome::CancelFailed { reason: "x".into() }, 50_100),
        );
        assert!(state.on_clock(100 + ORDER_STUCK_MS).is_ok());
        assert!(state.on_clock(100 + late).is_err(), "重撤不重新计时");
    }

    /// 没有基线时，第一条 Fill 从 0 起算（策略启动初期确实空仓）
    #[test]
    fn fill_without_baseline_starts_from_zero() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        apply(&mut state, &fill(Side::Short, dec!(1.5)));
        assert_eq!(state.position_book().position_size(EX), dec!(-1.5));
    }

    /// 持仓随 Fill **只累加数量**：买加、卖减、反手翻符号。
    ///
    /// 均价不在这里维护（也不在域模型里，见 `crate::domain::Position` 的文档）——
    /// 需要成本的策略自己从成交流记（口径由它自己定），记账则在 sim 账本里完成。
    #[test]
    fn position_accumulates_signed_size_across_fills() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 基线 2.0（快照请求时刻 0，fill() 的 local_ts=1 晚于它，不被过滤）
        state.seed_position(&position(dec!(2.0)), 0);

        // 同向加仓 2.0（成交价不影响持仓数量）
        apply(&mut state, &fill(Side::Long, dec!(2.0)));
        assert_eq!(
            state.position_book().position(EX).expect("position").size,
            dec!(4.0)
        );

        // 反向平掉 1.0（价格同样不影响数量）
        apply(&mut state, &fill(Side::Short, dec!(1.0)));
        assert_eq!(
            state.position_book().position(EX).expect("position").size,
            dec!(3.0)
        );

        // 反手：卖 5 -> 净空 2
        apply(&mut state, &fill(Side::Short, dec!(5.0)));
        assert_eq!(
            state.position_book().position(EX).expect("position").size,
            dec!(-2.0)
        );
    }
}

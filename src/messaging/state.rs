use crate::domain::{Exchange, FundingRate, IndexPrice, MarkPrice, Order, OrderStatus, Position, Quantity, Symbol, Timestamp, BBO};
use crate::messaging::event::{MarketData, OrderRequestResult, RequestOutcome, SymbolAccountData};
use std::collections::HashMap;

/// 一张单「等交易所」超过这么久就停机（见 [`SymbolOrders::check_stuck`]）。
///
/// 三种等待：下单请求还没有结论（REST 自带 10s 超时，结论必到）、撤单已发出但交易所没推终态、
/// 终态到了但成交没到齐。正常都在秒级以内；一分钟还没等到，说明回报丢了或解析错了 —— 跟踪表从此
/// 说不清这张单和这个币的持仓，继续交易会在旁边重复挂单、按错的持仓补腿，故停机交人工核对。
/// （此前是 60s 未确认就静默删单、确认迟到再重建 —— 删掉的单若还活着，策略会在旁边再挂一张。）
pub const ORDER_STUCK_MS: u64 = 60_000;

/// 成交是否到齐的相对容差：逐笔成交与累计量各自从张折成币（OKX），浮点和会差在末位
const FILL_TOLERANCE: f64 = 1e-9;

/// 本策略发出的一张单在本地的跟踪记录。
///
/// 策略发出时登记，**终态且成交都已入账**才移出：交易所说的累计成交（`reported_filled`）由按客户端
/// 订单号收到的成交（`filled`）加满。所以表里的单只有两种：还开着的（[`Self::is_open`]，可能再成交），
/// 和已结束、成交还在路上的 —— 后者在时，该币的持仓还会变。表空了，持仓就是这些单的最终结果。
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub order: Order,
    /// 交易所说的最新状态；`Created` = 下单请求还没有任何结论。到终态后不再被改写
    pub status: OrderStatus,
    pub created_at: Timestamp,
    /// 按客户端订单号收到的成交之和（币）
    pub filled: Quantity,
    /// 交易所回报里的累计成交（币，取见过的最大值）
    pub reported_filled: Quantity,
    /// 撤单进行到哪一步
    pub cancel: CancelState,
    /// 第一次发撤单的时刻。此后单该在 [`ORDER_STUCK_MS`] 内到终态，不管中间撤单失败、重撤了几次
    cancel_first_sent_at: Option<Timestamp>,
    /// 收到终态的时刻
    terminal_at: Option<Timestamp>,
}

/// 一张单的撤单进行到哪一步
#[derive(Debug, Clone, PartialEq)]
pub enum CancelState {
    /// 没撤过
    None,
    /// 撤单已发出：等交易所推终态，或撤单失败的结论。期间框架不再重发（见 `StrategyRunner`）
    Sent,
    /// 撤单请求失败。单若还开着，隔多久重撤由策略定；但从第一次撤起 [`ORDER_STUCK_MS`] 内没到终态，框架停机
    Failed { at: Timestamp, reason: String },
}

impl PendingOrder {
    /// 交易所还在撮合它（没到终态）：可能再成交，也只有它能撤
    pub fn is_open(&self) -> bool {
        !self.status.is_terminal()
    }

    /// 成交都已入账：本地收到的成交加起来不少于交易所说的累计量
    fn fills_settled(&self) -> bool {
        self.reported_filled - self.filled <= FILL_TOLERANCE * self.order.quantity.max(self.reported_filled)
    }

    /// 在等交易所的什么、从什么时候起；`None` = 没在等（挂着、没撤过）。
    /// 已终态的单还在表里，就是成交没到齐（到齐即移出）。撤过的单从第一次撤起算：撤单一直失败、
    /// 交易所又不推终态时，策略会一直重撤（HL 白耗操作额度）、该币也一直挂不了新单（#767）。
    fn waiting_on_exchange(&self) -> Option<(Timestamp, &'static str)> {
        if let Some(at) = self.terminal_at {
            return Some((at, "终态已到、成交没到齐"));
        }
        if self.status == OrderStatus::Created {
            return Some((self.created_at, "下单请求没有结论"));
        }
        self.cancel_first_sent_at.map(|at| (at, "撤单发出后交易所没推终态"))
    }
}


/// 单个交易对的**行情**投影：各所的盘口 / 标记价 / 指数价 / 资金费率。
///
/// 三个投影（行情 / 持仓 / 挂单）分开的理由见 `docs/architecture.md` V1：它们的消费者
/// 互不重叠 —— 对账只要持仓，策略要行情 + 持仓 + 挂单，观测还要账户。焊在一个结构里的
/// 直接后果是"想只查持仓"做不到，只能整份复制。
#[derive(Debug, Clone, Default)]
pub struct SymbolMarket {
    pub funding_rates: HashMap<Exchange, FundingRate>,
    pub bbos: HashMap<Exchange, BBO>,
    pub mark_prices: HashMap<Exchange, MarkPrice>,
    pub index_prices: HashMap<Exchange, IndexPrice>,
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

/// 单个交易对的**挂单**投影：本策略发出、还没结算完的单（见 [`PendingOrder`]）。
///
/// 输入四种：策略发单（[`Self::add`]）、策略撤单（[`Self::mark_cancel_sent`]）、交易所的订单回报与成交、
/// 本地 REST 请求的结论（[`OrderRequestResult`]）。后两类走不同的路径，到达顺序没有保证，所以每条
/// 规则都写成与顺序无关的形态：终态不被改写、下单受理只把 `Created` 往前推、成交与累计量谁先到都
/// 一样算。不在表里的单的回报一律不重建记录。
#[derive(Debug, Clone, Default)]
pub struct SymbolOrders {
    /// client_order_id → 记录
    pub(crate) orders: HashMap<String, PendingOrder>,
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
    /// 获取统一时间基准（所有交易所中最近的结算时间和当前时间）
    ///
    /// 返回 (base_settle_time, current_time)
    fn unified_time_base(&self) -> Option<(Timestamp, Timestamp)> {
        if self.funding_rates.is_empty() {
            return None;
        }

        // 找出最近的 next_settle_time
        let min_settle_time = self
            .funding_rates
            .values()
            .map(|r| r.next_settle_time)
            .min()?;

        // 使用最新的 timestamp 作为当前时间
        let current_time = self
            .funding_rates
            .values()
            .map(|r| r.timestamp)
            .max()?;

        Some((min_settle_time, current_time))
    }

    /// 获取日化费率最高的交易所 (适合做空)
    ///
    /// 使用统一时间基准计算日化费率，确保跨交易所比较公平
    pub fn best_short_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        let (base_settle_time, current_time) = self.unified_time_base()?;

        self.funding_rates
            .iter()
            .max_by(|a, b| {
                let a_daily = a.1.daily_rate_with_base_time(base_settle_time, current_time);
                let b_daily = b.1.daily_rate_with_base_time(base_settle_time, current_time);
                a_daily.total_cmp(&b_daily)
            })
            .map(|(e, r)| (*e, r))
    }

    /// 获取日化费率最低的交易所 (适合做多)
    ///
    /// 使用统一时间基准计算日化费率，确保跨交易所比较公平
    pub fn best_long_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        let (base_settle_time, current_time) = self.unified_time_base()?;

        self.funding_rates
            .iter()
            .min_by(|a, b| {
                let a_daily = a.1.daily_rate_with_base_time(base_settle_time, current_time);
                let b_daily = b.1.daily_rate_with_base_time(base_settle_time, current_time);
                a_daily.total_cmp(&b_daily)
            })
            .map(|(e, r)| (*e, r))
    }

    /// 获取某个交易所的 BBO
    pub fn bbo(&self, exchange: Exchange) -> Option<&BBO> {
        self.bbos.get(&exchange)
    }

    /// 获取某个交易所的标记价格
    pub fn mark_price(&self, exchange: Exchange) -> Option<&MarkPrice> {
        self.mark_prices.get(&exchange)
    }

    /// 获取某个交易所的指数价格
    pub fn index_price(&self, exchange: Exchange) -> Option<&IndexPrice> {
        self.index_prices.get(&exchange)
    }

    /// 行情类事件的状态转移（BBO/标记价/指数价/资金费率入缓存，其余策略自取）
    pub fn apply(&mut self, symbol: &Symbol, data: &MarketData) {
        match data {
            MarketData::FundingRate(rate) => {
                self.funding_rates.insert(rate.exchange, rate.clone());
            }
            MarketData::BBO(bbo) => {
                self.bbos.insert(bbo.exchange, bbo.clone());
            }
            MarketData::MarketTrade(_) => {
                // 公共成交印记仅作市场信号 (策略自取)，不修改聚合状态
            }
            MarketData::MarkPrice(mp) => {
                self.mark_prices.insert(mp.exchange, mp.clone());
            }
            MarketData::IndexPrice(ip) => {
                self.index_prices.insert(ip.exchange, ip.clone());
            }
            MarketData::Candle(_) | MarketData::HistoryCandles(_) => {
                // K线数据由策略层处理，SymbolState 不存储
            }
            MarketData::BorrowFee(_) => {
                // 券源读数由策略自行消费，SymbolState 不缓存
            }
            // 其余变体无 symbol（Status/汇率/Clock）或由 StateManager 提前拦截（Custom），
            // 到达即路由 bug：记录后忽略（不 panic）
            MarketData::ExchangeStatus { .. }
            | MarketData::ExchangeRate(_)
            | MarketData::Clock
            | MarketData::Custom(_) => {
                tracing::error!(
                    %symbol,
                    "全局行情事件错误地进入 SymbolState（路由 bug），忽略"
                );
            }
        }
    }
}

impl SymbolPositions {
    /// 累加一笔成交。**持仓维护的增量输入只有这一条路。**
    ///
    /// Fill 涵盖策略单、手动单、以及强平/ADL —— 三者都以 fill 形式经私有成交流到达，
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
                    fill_size = fill.size,
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
            fill_size = fill.size,
            fill_price = fill.price,
            reason = ?fill.reason,
            new_position_size = pos.size,
            "Updated position on fill"
        );
    }

    /// **全部**腿，含未 seed 的（供观测镜像发布持仓：要的是"账上有什么"，
    /// 而不是"哪些可参与对账"。对外快照请用 [`Self::seeded_positions`]）。
    pub fn all(&self) -> impl Iterator<Item = &Position> {
        self.positions.values()
    }

    /// 写入持仓**基线**（投产握手的载荷，不是事件 —— 见 [`crate::messaging::PositionBaseline`]）。
    ///
    /// 持仓维护模型：**一次性基线 + 之后全程由 Fill 累加**。基线只能写一次，因为快照与
    /// 实时 Fill 流之间存在竞态：快照可能已含某笔成交，其 Fill 稍后才到 —— 若覆写后再叠加
    /// 就会重复计算。为此：
    ///
    /// - `snapshot_req_ts` 是 REST 快照的**请求**时刻。在此之前送达（`local_ts <=`）的
    ///   Fill 必已含在快照里，seed 之后到达时被 [`Self::apply`] 的 Fill 分支丢弃 ——
    ///   这条过滤规则对所有基线消费者（executor、对账镜像、观测镜像）同一份。
    /// - 同一所第二次 seed **静默跳过**并返回 `false`：镜像在每次投产注册时都会收到
    ///   该批 symbol 的基线（同一 symbol 再次投产会重复），首次写入后的重复是流程的正常形态。
    ///   （历史上"重复基线 = 违约"针对的是总线上适配层乱发；基线退出事件流后，
    ///   seed 只能来自投产握手，违约面已不存在。）
    ///
    /// # 如实声明的残余窗口（没有交易所侧序号无法根除）
    ///
    /// - 成交发生在快照**之前**（已含在快照）、Fill 在快照请求之后才送达
    ///   （`local_ts > snapshot_req_ts`）—— 会双计，窗口 = 交易所撮合到 Fill 送达的时延。
    /// - 同一 symbol 再次投产时新 executor 用**新**快照 seed 而镜像沿用旧账本：新快照到注册完成之间的
    ///   外部成交会造成 executor 与镜像分歧（见 `activate_executors` 的窗口清单；
    ///   实盘 executor 与镜像的分歧由持仓账本检出并停机，#694）。
    ///
    /// 都只可能来自投产瞬间的外部/手动成交，与旧的总线投递方案等宽。
    pub fn seed(
        &mut self,
        symbol: &Symbol,
        position: &Position,
        snapshot_req_ts: Timestamp,
    ) -> bool {
        match self.seeded_at.get(&position.exchange) {
            Some(_) => {
                tracing::debug!(
                    %symbol,
                    exchange = %position.exchange,
                    incoming_size = position.size,
                    "持仓基线已存在，跳过重复 seed（同一 symbol 再次投产时的正常形态）"
                );
                false
            }
            None => {
                // 覆写"未 seed 但已被 Fill 累加出的仓位"是合法的（快照权威；可达场景：
                // 前一批策略只订了别的所、本所的外部 Fill 已按 symbol 进入镜像累加），
                // 但覆写非零值必须可见 —— 静默覆写会掩盖"累加值与快照不一致"的线索。
                if let Some(existing) = self.positions.get(&position.exchange) {
                    if !existing.is_empty() {
                        tracing::warn!(
                            %symbol,
                            exchange = %position.exchange,
                            accumulated_size = existing.size,
                            snapshot_size = position.size,
                            "seed 覆写了此前由 Fill 累加出的非零仓位（快照权威）"
                        );
                    }
                }
                tracing::info!(
                    %symbol,
                    exchange = %position.exchange,
                    size = position.size,
                    snapshot_req_ts,
                    "Position baseline seeded"
                );
                self.seeded_at
                    .insert(position.exchange, snapshot_req_ts);
                self.positions.insert(position.exchange, position.clone());
                true
            }
        }
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

    /// 是否有持仓
    pub fn has_positions(&self) -> bool {
        self.positions.values().any(|p| !p.is_empty())
    }

    /// 获取某个交易所的仓位
    pub fn position(&self, exchange: Exchange) -> Option<&Position> {
        self.positions.get(&exchange)
    }

    /// 获取某个交易所的仓位大小。**无记录与空仓都返回 `0.0`**。
    ///
    /// 压平在"算下单量"这个用途上永远安全（理由见
    /// [`crate::strategy::SymbolView::position_size`]）；要拿"有没有这条记录"当交易前置
    /// 条件，用 [`Self::position`] —— 它保留了 `Option`，那里也写清了 `None` 的三种来路。
    ///
    /// 此前这里的注释写的是"无仓位记录等价于空仓，这是正确的业务语义"。那句话在下单量
    /// 场景下成立，但它没有限定用途，读起来像是断言两者在任何场景下都等价 —— 而模拟策略
    /// 首笔成交前、以及无凭证的所，恰恰是需要分辨的。
    pub fn position_size(&self, exchange: Exchange) -> f64 {
        self.positions.get(&exchange).map(|p| p.size).unwrap_or(0.0)
    }

    /// 获取多空仓位大小
    ///
    /// 返回 (多头总量, 空头总量):
    /// - 多头总量: 所有正向持仓之和（正数）
    /// - 空头总量: 所有负向持仓之和（负数）
    pub fn position_sizes(&self) -> (f64, f64) {
        let mut long_size = 0.0;
        let mut short_size = 0.0;

        for pos in self.positions.values() {
            if pos.size > 0.0 {
                long_size += pos.size;
            } else if pos.size < 0.0 {
                short_size += pos.size;
            }
        }

        (long_size, short_size)
    }
}

impl SymbolOrders {
    /// 登记策略刚发出的一张单（client_order_id 已分配）
    pub fn add(&mut self, order: Order, created_at: Timestamp) {
        let client_order_id = order.client_order_id.clone();
        self.orders.insert(client_order_id, PendingOrder {
            order,
            status: OrderStatus::Created,
            created_at,
            filled: 0.0,
            reported_filled: 0.0,
            cancel: CancelState::None,
            cancel_first_sent_at: None,
            terminal_at: None,
        });
    }

    /// 策略要撤这张单：记下「撤单已发」，返回 `Ok` 表示该发。
    ///
    /// 不该发的（`Err` 带原因）：不在表里、已到终态（撤不掉，发了白耗一次交易所操作）、下单请求还没结论、
    /// 上一次撤单还在等结论。
    ///
    /// 下单没结论时不撤：下单与撤单是两个独立请求，撤单可能先到交易所、报「单不存在」—— 白耗一次
    /// 操作（HL 有每日额度），单还要等策略的重撤间隔才撤掉（#769 实盘 HL 多挂了 2 秒）。受理的结论（或交易所
    /// 回报）一到策略就会被调用、可以再撤，只晚一个往返。
    pub fn mark_cancel_sent(&mut self, client_order_id: &str, now: Timestamp) -> Result<(), &'static str> {
        let Some(p) = self.orders.get_mut(client_order_id) else {
            return Err("不在跟踪表里");
        };
        if !p.is_open() {
            return Err("已到终态");
        }
        if p.status == OrderStatus::Created {
            return Err("下单请求还没结论");
        }
        if p.cancel == CancelState::Sent {
            return Err("上一次撤单还在等结论");
        }
        p.cancel = CancelState::Sent;
        p.cancel_first_sent_at.get_or_insert(now);
        Ok(())
    }

    /// 交易所的订单回报
    fn apply_update(&mut self, symbol: &Symbol, update: &crate::domain::OrderUpdate, local_ts: Timestamp) {
        let Some(client_id) = &update.client_order_id else {
            return; // 没有客户端订单号：不是本引擎的单
        };
        if update.status == OrderStatus::Created {
            // Created 是本地状态，交易所不会推；出现说明 codec 误映射
            tracing::error!(%symbol, exchange = %update.exchange, %client_id, "交易所回报了 Created（codec 映射异常），忽略");
            return;
        }
        let Some(p) = self.orders.get_mut(client_id) else {
            if !update.status.is_terminal() && update.exchange.owns_cli_order_id(client_id) {
                // 本引擎订单号、还开着、本地却没有：别的实例或上一个进程的单（启动时已撤净遗留挂单，
                // 正常不会出现）。不接管：它的成交照常进持仓，由策略按普通不平衡处理
                tracing::warn!(%symbol, exchange = %update.exchange, %client_id, status = ?update.status, "本引擎订单号的挂单不在跟踪表里，不接管");
            }
            return;
        };
        p.reported_filled = p.reported_filled.max(update.filled_quantity);
        if p.order.id.is_empty() {
            p.order.id = update.order_id.clone();
        }
        // 终态不被改写：同一频道内回报有序，这里只防将来有人把两条路径的回报混进来
        if p.is_open() {
            p.status = update.status.clone();
            if p.status.is_terminal() {
                p.terminal_at = Some(local_ts);
            }
        }
        self.settle(symbol, client_id);
    }

    /// 一笔成交计入它所属的单（持仓另由 [`SymbolPositions::apply_fill`] 累加）
    fn apply_fill(&mut self, symbol: &Symbol, fill: &crate::domain::Fill) {
        let Some(client_id) = &fill.client_order_id else {
            return;
        };
        if let Some(p) = self.orders.get_mut(client_id) {
            p.filled += fill.size;
            self.settle(symbol, client_id);
        }
    }

    /// 本地 REST 请求的结论。单已不在表里（交易所回报先到、已结算完）就不理。
    fn apply_request(&mut self, symbol: &Symbol, r: &OrderRequestResult, local_ts: Timestamp) {
        let Some(p) = self.orders.get_mut(&r.client_order_id) else {
            return;
        };
        match &r.outcome {
            RequestOutcome::Placed { order_id } => {
                if p.order.id.is_empty() {
                    p.order.id = order_id.clone();
                }
                // 只把「没有结论」往前推；交易所已说过的状态（含终态）不动
                if p.status == OrderStatus::Created {
                    p.status = OrderStatus::Pending;
                }
            }
            RequestOutcome::PlaceRejected { reason } => {
                // 确定没挂上（实盘出口只在交易所没对这张单说过话时才报拒单），也就不会有成交。
                // 交易所已对这张单说过话（拒单结论和交易所回报在出口前后脚到）时以交易所为准：
                // 单留在表里由回报走完生命周期，不能移出后让策略在旁边再挂一张（#764 审查）
                if p.status == OrderStatus::Created {
                    tracing::info!(%symbol, exchange = %r.exchange, client_order_id = %r.client_order_id, %reason, "下单被拒，移出跟踪表");
                    self.orders.remove(&r.client_order_id);
                } else {
                    tracing::error!(%symbol, exchange = %r.exchange, client_order_id = %r.client_order_id, %reason, status = ?p.status,
                        "下单报被拒，但交易所已回报过这张单：以交易所回报为准，不移出");
                }
            }
            RequestOutcome::CancelFailed { reason } => {
                p.cancel = CancelState::Failed { at: local_ts, reason: reason.clone() };
            }
        }
    }

    /// 终态且成交到齐就移出
    fn settle(&mut self, symbol: &Symbol, client_id: &str) {
        if self.orders.get(client_id).is_some_and(|p| !p.is_open() && p.fills_settled()) {
            let p = self.orders.remove(client_id).expect("刚查过");
            if p.filled - p.reported_filled > FILL_TOLERANCE * p.order.quantity.max(p.reported_filled) {
                // 持仓对账随后会发现漂移并停机；这里先记下是哪张单多收了成交，便于排查（#764 审查）
                tracing::error!(%symbol, exchange = %p.order.exchange, %client_id, filled = p.filled, reported_filled = p.reported_filled,
                    "本地收到的成交多于交易所说的累计成交量（成交可能被重复计入）");
            }
            tracing::info!(%symbol, exchange = %p.order.exchange, %client_id, status = ?p.status, filled = p.filled, "订单结算完，移出跟踪表");
        }
    }

    /// 有单「等交易所」超过 [`ORDER_STUCK_MS`] 就报错（调用方停机）
    fn check_stuck(&self, symbol: &Symbol, now: Timestamp) -> Result<(), String> {
        for (client_id, p) in &self.orders {
            if let Some((since, what)) = p.waiting_on_exchange() {
                let waited = now.saturating_sub(since);
                if waited > ORDER_STUCK_MS {
                    return Err(format!(
                        "{symbol} {} 订单 {client_id}（交易所号 {}）{what}已 {waited}ms：状态 {:?}，收到成交 {}，交易所说累计 {}，撤单 {:?}。\
                         跟踪表说不清这张单和持仓，停机交人工核对挂单与持仓",
                        p.order.exchange, p.order.id, p.status, p.filled, p.reported_filled, p.cancel
                    ));
                }
            }
        }
        Ok(())
    }

    /// 是否还有没结算完的单（开着的，或终态了、成交还在路上的）
    pub fn has_pending(&self) -> bool {
        !self.orders.is_empty()
    }

    /// 全部没结算完的单
    pub fn iter(&self) -> impl Iterator<Item = &PendingOrder> {
        self.orders.values()
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
    pub fn apply_market(&mut self, data: &MarketData) {
        debug_assert_eq!(data.symbol(), Some(&self.symbol), "行情被路由到了别的 symbol 状态");
        self.market.apply(&self.symbol, data);
    }

    /// 币级账户事件的状态转移（挂单跟踪 + 持仓累加）。调用方已按 symbol 路由。
    ///
    /// 只收 [`SymbolAccountData`]：所级账户事件在类型上就到不了这里。
    pub fn apply_account(&mut self, data: &SymbolAccountData, local_ts: Timestamp) {
        debug_assert_eq!(data.symbol(), &self.symbol, "账户事件被路由到了别的 symbol 状态");
        match data {
            SymbolAccountData::OrderUpdate(update) => {
                tracing::info!(
                    symbol = %self.symbol,
                    exchange = %update.exchange,
                    order_id = %update.order_id,
                    client_order_id = ?update.client_order_id,
                    status = ?update.status,
                    filled_quantity = update.filled_quantity,
                    "Updating order status"
                );
                self.order_book.apply_update(&self.symbol, update, local_ts);
            }
            SymbolAccountData::OrderRequest(r) => {
                self.order_book.apply_request(&self.symbol, r, local_ts);
            }
            SymbolAccountData::Fill(fill) => {
                // 不相交字段借用：symbol 与 position_book 是两个字段，无需为每条 Fill 克隆
                self.position_book.apply_fill(&self.symbol, fill, local_ts);
                self.order_book.apply_fill(&self.symbol, fill);
            }
            SymbolAccountData::FundingFee(_) => {
                // 资费事件不修改本地 symbol 状态，下游策略自行去重与统计
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

    pub fn seed_position(&mut self, position: &Position, snapshot_req_ts: Timestamp) -> bool {
        self.position_book.seed(&self.symbol, position, snapshot_req_ts)
    }

    pub fn add_pending_order(&mut self, order: Order, created_at: Timestamp) {
        self.order_book.add(order, created_at);
    }

    /// 策略要撤这张单，见 [`SymbolOrders::mark_cancel_sent`]
    pub fn mark_cancel_sent(&mut self, client_order_id: &str, now: Timestamp) -> Result<(), &'static str> {
        self.order_book.mark_cancel_sent(client_order_id, now)
    }

    /// 见 [`SymbolOrders::check_stuck`]
    pub fn check_stuck_orders(&self, now: Timestamp) -> Result<(), String> {
        self.order_book.check_stuck(&self.symbol, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Side;

    const SYMBOL: &str = "BTC";

    // ===== 持仓维护模型：基线只来一次 + Fill 累加 + 对账读数不写入 =====

    const EX: Exchange = Exchange::Binance;

    /// 一条币级账户事件及其送达时刻
    type Ev = (SymbolAccountData, Timestamp);

    fn apply(state: &mut SymbolState, (data, local_ts): &Ev) {
        state.apply_account(data, *local_ts);
    }

    fn position(size: f64) -> Position {
        Position {
            exchange: EX,
            symbol: SYMBOL.to_string(),
            size,
        }
    }

    fn fill(side: Side, size: f64) -> Ev {
        fill_at(EX, side, size, 1)
    }

    fn fill_at(exchange: Exchange, side: Side, size: f64, local_ts: u64) -> Ev {
        (
            SymbolAccountData::Fill(crate::domain::Fill {
                exchange,
                symbol: SYMBOL.to_string(),
                side,
                price: 100.0,
                size,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: local_ts,
                fee: 0.0,
                reason: crate::domain::FillReason::Normal,
            }),
            local_ts,
        )
    }

    /// 基线写入一次，之后**只**由 Fill 累加。
    #[test]
    fn baseline_initializes_then_fills_accumulate() {
        let mut state = SymbolState::new(SYMBOL.to_string());

        assert!(state.seed_position(&position(2.0), 0), "首次 seed 应成功");
        assert_eq!(state.position_book().position_size(EX), 2.0);

        apply(&mut state, &fill(Side::Long, 1.0));
        assert_eq!(state.position_book().position_size(EX), 3.0);

        apply(&mut state, &fill(Side::Short, 0.5));
        assert!((state.position_book().position_size(EX) - 2.5).abs() < 1e-12);
    }

    /// **Critical 回归防线**：第二次 seed 不得覆写持仓（幂等，返回 false）。
    ///
    /// 快照覆写 + 晚到的 Fill 会重复计算同一笔成交。同一 symbol 再次投产时镜像会收到重复基线，
    /// 全靠这条幂等判据兜住存量。
    #[test]
    fn second_baseline_never_overwrites_position() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        state.seed_position(&position(2.0), 0);
        apply(&mut state, &fill(Side::Long, 1.0));
        assert_eq!(state.position_book().position_size(EX), 3.0);

        // 第二次 seed（哪怕数值"看起来更新"）也不得覆写
        assert!(!state.seed_position(&position(3.0), 5), "重复 seed 应返回 false");
        assert_eq!(
            state.position_book().position_size(EX),
            3.0,
            "第二次 seed 覆写了持仓——快照与 Fill 流叠加会重复计算成交"
        );

        // 覆写若发生，这条 Fill 会把仓位推到 4.0 之外
        apply(&mut state, &fill(Side::Long, 1.0));
        assert_eq!(state.position_book().position_size(EX), 4.0);
    }

    /// **防双计**：seed 之后到达、但送达时刻早于快照请求时刻的 Fill 已含在快照里，丢弃；
    /// 之后的照常累加。未 seed 的所不受过滤影响。
    #[test]
    fn fills_covered_by_snapshot_are_dropped() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 快照请求时刻 t=10
        state.seed_position(&position(2.0), 10);

        // t=1 送达（fill() 的 local_ts=1 <= 10）：已含在快照里，丢弃
        apply(&mut state, &fill(Side::Long, 1.0));
        assert_eq!(state.position_book().position_size(EX), 2.0, "快照已含的 Fill 不得再累加");

        // t=11 送达：快照之后的新成交，累加
        apply(&mut state, &fill_at(EX, Side::Long, 1.0, 11));
        assert_eq!(state.position_book().position_size(EX), 3.0);
    }

    /// **seed 过滤按所隔离**：只 seed 了一个所时，另一个所的早时刻 Fill 不得被误过滤
    /// （guard 按 `fill.exchange` 查表，不是按 symbol 全局生效）
    #[test]
    fn seed_filter_is_scoped_to_its_exchange() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 只 seed Binance（快照请求时刻 t=10）
        state.seed_position(&position(2.0), 10);

        // OKX 未 seed：t=1 送达的 Fill 照常从 0 累加，不受 Binance 的过滤时刻影响
        apply(&mut state, &fill_at(Exchange::OKX, Side::Long, 1.5, 1));
        assert_eq!(state.position_book().position_size(Exchange::OKX), 1.5, "未 seed 的所不得被误过滤");
        assert_eq!(state.position_book().position_size(EX), 2.0, "Binance 腿不受影响");
    }

    // ===== 订单跟踪：结算完才移出、与到达顺序无关、不重建 =====

    const CID: &str = "cid-1";

    fn tracked_order(qty: f64) -> Order {
        Order {
            id: String::new(),
            exchange: EX,
            symbol: SYMBOL.to_string(),
            side: Side::Long,
            order_type: crate::domain::OrderType::Limit { price: 100.0, tif: crate::domain::TimeInForce::GTC },
            quantity: qty,
            reduce_only: false,
            client_order_id: CID.to_string(),
        }
    }

    /// 登记一张 1.0 的单（t=0）
    fn with_order() -> SymbolState {
        let mut state = SymbolState::new(SYMBOL.to_string());
        state.add_pending_order(tracked_order(1.0), 0);
        state
    }

    fn update(cid: &str, status: OrderStatus, filled: f64, ts: u64) -> Ev {
        (
            SymbolAccountData::OrderUpdate(crate::domain::OrderUpdate {
                order_id: "ex-1".to_string(),
                client_order_id: Some(cid.to_string()),
                exchange: EX,
                symbol: SYMBOL.to_string(),
                side: Side::Long,
                status,
                quantity: 1.0,
                filled_quantity: filled,
            }),
            ts,
        )
    }

    fn own_fill(size: f64, ts: u64) -> Ev {
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

    fn the_order(state: &SymbolState) -> Option<&PendingOrder> {
        state.order_book().iter().find(|p| p.order.client_order_id == CID)
    }

    /// **#758 的漏洞**：HL 的终态回报先于成交到达时，单必须留在表里（不再开着），直到成交加满。
    /// 此前终态一到就移出：策略看到「没挂单、持仓还平」，在同所再挂一张。
    #[test]
    fn terminal_before_fills_stays_until_fills_add_up() {
        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::Filled, 1.0, 1));
        let p = the_order(&state).expect("成交没到齐，还在表里");
        assert!(!p.is_open());

        apply(&mut state, &own_fill(0.4, 2));
        assert!(the_order(&state).is_some(), "只到了 0.4");
        apply(&mut state, &own_fill(0.6, 3));
        assert!(!state.order_book().has_pending(), "成交加满即移出");
        assert!((state.position_book().position_size(EX) - 1.0).abs() < 1e-12);
    }

    /// 反过来：成交先到、终态后到，终态一到即移出；没有成交的撤单也是终态即移出
    #[test]
    fn fills_first_or_no_fills_settle_on_terminal() {
        let mut state = with_order();
        apply(&mut state, &own_fill(1.0, 1));
        assert!(the_order(&state).is_some_and(|p| p.is_open()), "没终态前还开着");
        apply(&mut state, &update(CID, OrderStatus::Filled, 1.0, 2));
        assert!(!state.order_book().has_pending());

        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::Cancelled, 0.0, 1));
        assert!(!state.order_book().has_pending());
    }

    /// 部分成交后撤单：累计量以交易所说的为准，不是订单总量；部分成交回报里的累计量也算
    #[test]
    fn partial_fill_then_cancel_waits_for_the_reported_amount() {
        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::PartiallyFilled, 0.3, 1));
        apply(&mut state, &update(CID, OrderStatus::Cancelled, 0.0, 2)); // 有的所撤单回报不带累计，取见过的最大
        assert!(the_order(&state).is_some(), "0.3 的成交还没到");
        apply(&mut state, &own_fill(0.3, 3));
        assert!(!state.order_book().has_pending());
    }

    /// 终态不被改写：下单受理的本地结论晚于交易所的终态到达，不能把它拉回「挂着」
    #[test]
    fn placed_after_terminal_does_not_reopen() {
        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::Filled, 1.0, 1));
        apply(&mut state, &request(RequestOutcome::Placed { order_id: "ex-1".into() }, 2));
        assert!(the_order(&state).is_some_and(|p| !p.is_open()));
    }

    /// 下单受理：回填订单号、Created → Pending；被拒：移出；撤单失败：记在单上、单还开着
    #[test]
    fn request_results_drive_the_record() {
        let mut state = with_order();
        apply(&mut state, &request(RequestOutcome::Placed { order_id: "77".into() }, 1));
        let p = the_order(&state).unwrap();
        assert_eq!((p.status.clone(), p.order.id.as_str()), (OrderStatus::Pending, "77"));

        state.mark_cancel_sent(CID, 2).unwrap();
        assert_eq!(state.mark_cancel_sent(CID, 3), Err("上一次撤单还在等结论"), "撤单在途不重发");
        apply(&mut state, &request(RequestOutcome::CancelFailed { reason: "busy".into() }, 4));
        let p = the_order(&state).unwrap();
        assert!(p.is_open());
        assert_eq!(p.cancel, CancelState::Failed { at: 4, reason: "busy".into() });
        assert!(state.mark_cancel_sent(CID, 5).is_ok(), "失败后可以再撤");

        let mut state = with_order();
        apply(&mut state, &request(RequestOutcome::PlaceRejected { reason: "margin".into() }, 1));
        assert!(!state.order_book().has_pending());
    }

    /// 拒单结论晚于交易所回报到：以交易所为准，单不移出（否则策略会在旁边再挂一张）
    #[test]
    fn rejection_after_exchange_update_keeps_the_order() {
        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::Pending, 0.0, 1));
        apply(&mut state, &request(RequestOutcome::PlaceRejected { reason: "margin".into() }, 2));
        assert!(the_order(&state).is_some_and(|p| p.is_open()));
    }

    /// 下单没结论、已到终态的单不发撤单（撤不掉，HL 还白耗一次操作额度）；不在表里的也不发
    #[test]
    fn no_cancel_for_finished_or_unknown_orders() {
        let mut state = with_order();
        assert_eq!(state.mark_cancel_sent(CID, 1), Err("下单请求还没结论"), "撤单会先于下单到交易所");
        assert_eq!(the_order(&state).unwrap().cancel, CancelState::None, "没记成已撤，受理后还能撤");

        apply(&mut state, &update(CID, OrderStatus::Filled, 1.0, 1));
        assert_eq!(state.mark_cancel_sent(CID, 2), Err("已到终态"));
        assert_eq!(state.mark_cancel_sent("other", 2), Err("不在跟踪表里"));
    }

    /// 不在表里的单的回报不重建记录：本引擎的、别人的、终态的、开着的都一样
    #[test]
    fn updates_of_untracked_orders_never_create_records() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        let own = EX.new_cli_order_id();
        for cid in [own.as_str(), "web_1a2b3c", "android_9f8e"] {
            apply(&mut state, &update(cid, OrderStatus::Pending, 0.0, 1));
            apply(&mut state, &update(cid, OrderStatus::PartiallyFilled, 0.5, 2));
        }
        assert!(!state.order_book().has_pending());
    }

    /// 三种「等交易所」超过 [`ORDER_STUCK_MS`] 都报错；挂着不动不算；撤过的单从第一次撤起算，中间撤单失败、重撤不重新计时
    #[test]
    fn waiting_on_the_exchange_too_long_is_an_error() {
        let late = ORDER_STUCK_MS + 1;
        // 下单请求没有结论
        let state = with_order();
        assert!(state.check_stuck_orders(ORDER_STUCK_MS).is_ok());
        assert!(state.check_stuck_orders(late).is_err());

        // 挂着：多久都不算
        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::Pending, 0.0, 1));
        assert!(state.check_stuck_orders(10 * late).is_ok());

        // 撤单发出后交易所没推终态
        state.mark_cancel_sent(CID, 100).unwrap();
        assert!(state.check_stuck_orders(100 + ORDER_STUCK_MS).is_ok());
        assert!(state.check_stuck_orders(100 + late).is_err());
        // 撤单一直失败、反复重撤：仍从第一次撤起算（此前撤单失败后不算在等，该币会一直卡在只撤不挂）
        apply(&mut state, &request(RequestOutcome::CancelFailed { reason: "x".into() }, 200));
        assert!(state.check_stuck_orders(100 + late).is_err());
        state.mark_cancel_sent(CID, 50_000).unwrap();
        apply(&mut state, &request(RequestOutcome::CancelFailed { reason: "x".into() }, 50_100));
        assert!(state.check_stuck_orders(100 + ORDER_STUCK_MS).is_ok());
        assert!(state.check_stuck_orders(100 + late).is_err(), "重撤不重新计时");

        // 终态到了、成交没到齐
        let mut state = with_order();
        apply(&mut state, &update(CID, OrderStatus::Filled, 1.0, 1_000));
        assert!(state.check_stuck_orders(1_000 + ORDER_STUCK_MS).is_ok());
        assert!(state.check_stuck_orders(1_000 + late).is_err());
    }

    /// 没有基线时，第一条 Fill 从 0 起算（策略启动初期确实空仓）
    #[test]
    fn fill_without_baseline_starts_from_zero() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        apply(&mut state, &fill(Side::Short, 1.5));
        assert_eq!(state.position_book().position_size(EX), -1.5);
    }

    /// 持仓随 Fill **只累加数量**：买加、卖减、反手翻符号。
    ///
    /// 均价不在这里维护（也不在域模型里，见 `crate::domain::Position` 的文档）——
    /// 需要成本的策略自己从成交流记（口径由它自己定），记账则在 sim 账本里完成。
    #[test]
    fn position_accumulates_signed_size_across_fills() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 基线 2.0（快照请求时刻 0，fill() 的 local_ts=1 晚于它，不被过滤）
        state.seed_position(&position(2.0), 0);

        // 同向加仓 2.0（成交价不影响持仓数量）
        apply(&mut state, &fill(Side::Long, 2.0));
        assert!((state.position_book().position(EX).expect("position").size - 4.0).abs() < 1e-12);

        // 反向平掉 1.0（价格同样不影响数量）
        apply(&mut state, &fill(Side::Short, 1.0));
        assert!((state.position_book().position(EX).expect("position").size - 3.0).abs() < 1e-12);

        // 反手：卖 5 -> 净空 2
        apply(&mut state, &fill(Side::Short, 5.0));
        assert!((state.position_book().position(EX).expect("position").size + 2.0).abs() < 1e-12);
    }

}

use crate::domain::{Balance, BorrowFee, Candle, Exchange, ExchangeRate, Fill, FundingFee, FundingRate, Greeks, IndexPrice, MarkPrice, MarketStatus, MarketTrade, OrderUpdate, Position, Symbol, Timestamp, BBO};

/// 统一的交易所事件
///
/// 设计原则：
/// - exchange_ts: 交易所推送的时间戳
/// - local_ts: 本地接收时间戳
/// - data: 具体的事件数据
#[derive(Debug, Clone)]
pub struct IncomeEvent {
    /// 交易所时间戳
    pub exchange_ts: Timestamp,
    /// 本地接收时间戳
    pub local_ts: Timestamp,
    /// 事件数据
    pub data: ExchangeEventData,
}

/// 事件数据类型
#[derive(Debug, Clone)]
pub enum ExchangeEventData {
    FundingRate(FundingRate),
    BBO(BBO),
    /// 公共成交印记 (市场匿名成交)；回测撮合的成交来源，实盘暂无该馈送
    MarketTrade(MarketTrade),
    MarkPrice(MarkPrice),
    IndexPrice(IndexPrice),
    /// 持仓**基线**：起始持仓，写入 [`crate::messaging::SymbolState`]。
    ///
    /// # 唯一合法产地：`ManagerActor` 投产期的 REST 查询
    ///
    /// 持仓的维护模型是「一次性基线 + 之后全程由 [`Self::Fill`] 累加」。基线之所以只能来一次，
    /// 是因为持仓快照与实时 Fill 流之间存在竞态：快照可能已含某笔成交，而该成交的 Fill 稍后
    /// 才到；若用快照覆写后再叠加这笔晚到的 Fill，就会**重复计算**。
    ///
    /// # 两条投递路径，各自"只来一次"
    ///
    /// - **点对点**：投产时 `ManagerActor` 把基线直接投进新 executor 的邮箱，且在其注册进
    ///   事件流**之前** —— FIFO 保证基线先于任何 Fill 被处理。每个 executor 一生收一次。
    /// - **总线**：只喂引擎生命周期的镜像（对账/观测）。每个 `(exchange, symbol)` 在引擎
    ///   生命周期内至多发布一次（降级后再晋升不重发——镜像的账本在实例撤下期间也在跟 Fill）。
    ///   路由层对 executor 是 SkipExecutors，不会与点对点那份重复。
    ///
    /// # `exchange_ts` 的契约：快照的**请求**时刻
    ///
    /// 不是"交易所时间戳"。生产方（`ManagerActor` 投产期）在发起 REST 查询前取值写入；
    /// 消费方（对账镜像）据此过滤重放：在该时刻之前送达的 Fill 必已含在快照里，重放即双计。
    ///
    /// 在任一消费者处第二次到达即为违约（[`crate::messaging::SymbolState::apply`] 会打
    /// error 并忽略）。交易所适配层**一律不得**发布本事件。私有 WS 的持仓推送（OKX
    /// `positions`、Hyperliquid `clearinghouseState`、Binance `ACCOUNT_UPDATE.P`）都是
    /// **读数**而非基线，要用于校验请走 [`Self::PositionReport`]
    PositionBaseline(Position),
    /// 持仓**对账读数**：某交易所的**完整**持仓快照，只用于校验，绝不写入本地持仓。
    ///
    /// 由 [`crate::engine::PositionPollingActor`] 周期性 REST 拉取，交给
    /// [`crate::engine::PositionReconcileActor`] 与「基线 + Fill 累加」的结果比对。
    ///
    /// # 为什么是整份快照，而不是逐个 symbol 一条事件
    ///
    /// 对账最要抓的一类漂移是**本地有仓、交易所已经没了**（漏了一笔平仓成交、或强平的 Fill
    /// 没收到）。发现它只能靠「交易所没报告这个 symbol ⇒ 它空仓」这个推断，而该推断**只有
    /// 完整快照成立**：逐条发的话，某 symbol 不出现既可能是空仓、也可能是这次没查到，
    /// 二者无法区分 —— 那就退化成了增量推送，也就丢掉了选 REST 而非私有 WS 推送的理由。
    ///
    /// 因此 `positions` 必须是该所**全量**（空仓的 symbol 直接不在其中，由消费方按 0 处理），
    /// 缺一条就等于谎报一个空仓。
    PositionReport {
        exchange: Exchange,
        /// 该交易所的完整持仓列表（币本位，见 [`crate::domain::Quantity`]）
        positions: Vec<Position>,
    },
    OrderUpdate(OrderUpdate),
    /// 成交事件 (用于乐观更新仓位)
    Fill(Fill),
    Balance(Balance),
    /// 资费结算事件 (账户每次收/付资费时推送)
    FundingFee(FundingFee),
    /// 账户希腊值 (按币种)
    Greeks(Greeks),
    /// 账户信息 (净值 + 总持仓名义价值)
    AccountInfo {
        exchange: Exchange,
        /// 账户净值 (balance + unrealized_pnl)
        equity: f64,
        /// 账户总持仓名义价值 (用于计算杠杆率)
        notional: f64,
    },
    /// 交易所市场状态变更
    ExchangeStatus {
        exchange: Exchange,
        status: MarketStatus,
    },
    /// K线实时推送
    Candle(Candle),
    /// 历史K线批量数据（订阅时一次性推送）
    HistoryCandles(Vec<Candle>),
    /// 融券券源读数 (借券费 + 可借量)；由外部数据源（IBKR snapshot 轮询）产出，策略在 on_event 里消费
    BorrowFee(BorrowFee),
    /// 汇率读数 (货币对)；由外部数据源（IBKR snapshot 轮询）产出，策略在 on_event 里消费
    ExchangeRate(ExchangeRate),
    /// 时钟事件 (用于超时检测等定时任务)
    Clock,
}

/// 实盘分发层的事件路由方式（见 [`IncomeEvent::executor_routing`]）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventRouting {
    /// 按 (exchange, symbol) 路由到订阅了该 symbol 的 executor
    BySymbol { exchange: Exchange, symbol: Symbol },
    /// 广播给所有 executor（账户私有的广播事件仍按账户过滤）
    Broadcast,
    /// **不投递给任何 executor**：只服务总线上的非 executor 订阅者（对账/观测镜像）
    SkipExecutors,
}

impl IncomeEvent {
    /// 获取事件关联的 Symbol
    pub fn symbol(&self) -> Option<&Symbol> {
        match &self.data {
            ExchangeEventData::FundingRate(rate) => Some(&rate.symbol),
            ExchangeEventData::BBO(bbo) => Some(&bbo.symbol),
            ExchangeEventData::MarketTrade(t) => Some(&t.symbol),
            ExchangeEventData::MarkPrice(mp) => Some(&mp.symbol),
            ExchangeEventData::IndexPrice(ip) => Some(&ip.symbol),
            ExchangeEventData::PositionBaseline(pos) => Some(&pos.symbol),
            ExchangeEventData::OrderUpdate(update) => Some(&update.symbol),
            ExchangeEventData::Fill(fill) => Some(&fill.symbol),
            ExchangeEventData::FundingFee(fee) => Some(&fee.symbol),
            ExchangeEventData::BorrowFee(bf) => Some(&bf.symbol),
            ExchangeEventData::Candle(candle) => Some(&candle.symbol),
            ExchangeEventData::HistoryCandles(candles) => candles.first().map(|c| &c.symbol),
            ExchangeEventData::Balance(_)
            | ExchangeEventData::Greeks(_)
            | ExchangeEventData::AccountInfo { .. }
            | ExchangeEventData::ExchangeStatus { .. }
            | ExchangeEventData::ExchangeRate(_)
            // 对账读数是按**交易所**的整份快照，没有单一 symbol 可言（见变体文档）
            | ExchangeEventData::PositionReport { .. }
            | ExchangeEventData::Clock => None,
        }
    }

    /// 获取事件来源交易所
    pub fn exchange(&self) -> Option<Exchange> {
        match &self.data {
            ExchangeEventData::FundingRate(rate) => Some(rate.exchange),
            ExchangeEventData::BBO(bbo) => Some(bbo.exchange),
            ExchangeEventData::MarketTrade(t) => Some(t.exchange),
            ExchangeEventData::MarkPrice(mp) => Some(mp.exchange),
            ExchangeEventData::IndexPrice(ip) => Some(ip.exchange),
            ExchangeEventData::PositionBaseline(pos) => Some(pos.exchange),
            ExchangeEventData::OrderUpdate(update) => Some(update.exchange),
            ExchangeEventData::Fill(fill) => Some(fill.exchange),
            ExchangeEventData::Candle(candle) => Some(candle.exchange),
            ExchangeEventData::HistoryCandles(candles) => candles.first().map(|c| c.exchange),
            ExchangeEventData::Balance(bal) => Some(bal.exchange),
            ExchangeEventData::FundingFee(fee) => Some(fee.exchange),
            ExchangeEventData::Greeks(g) => Some(g.exchange),
            ExchangeEventData::BorrowFee(bf) => Some(bf.exchange),
            ExchangeEventData::ExchangeRate(er) => Some(er.exchange),
            ExchangeEventData::AccountInfo { exchange, .. } => Some(*exchange),
            ExchangeEventData::ExchangeStatus { exchange, .. } => Some(*exchange),
            ExchangeEventData::PositionReport { exchange, .. } => Some(*exchange),
            ExchangeEventData::Clock => None,
        }
    }

    /// 路由键: `Some` 表示按 (exchange, symbol) 定向路由, `None` 表示广播给所有策略。
    /// 与 `StrategyRunner::accepts` 配套：全局事件广播、symbol 事件仅投订阅范围内的策略。
    pub fn routing(&self) -> Option<(Exchange, Symbol)> {
        match (self.exchange(), self.symbol()) {
            (Some(e), Some(s)) => Some((e, s.clone())),
            _ => None,
        }
    }

    /// 实盘分发层（IncomeProcessor -> executor）的路由方式。
    ///
    /// **路由知识的单一出处**：由 [`Self::routing`]（symbol/exchange 两个 match）推导，
    /// 仅对两类持仓事件显式例外 —— 新增事件变体时改 `symbol()`/`exchange()` 即可，
    /// 不存在需要手工同步的第二份 match（此前实盘分发层自带一份独立 match，新增变体
    /// 漏改一处的表现是"回测收得到、实盘收不到"，编译照过）。
    pub fn executor_routing(&self) -> EventRouting {
        match &self.data {
            // 对账读数只服务总线上的镜像（对账/观测），流进策略会绕过「基线 + Fill」模型
            ExchangeEventData::PositionReport { .. } => EventRouting::SkipExecutors,
            // 基线由 ManagerActor 点对点投递（注册前、先于一切流事件），总线那份只喂镜像；
            // 若再经分发层投递，executor 会收到重复基线（见 PositionBaseline 的文档）
            ExchangeEventData::PositionBaseline(_) => EventRouting::SkipExecutors,
            // 发布端保证 HistoryCandles 非空（空数组在发布前过滤）。空到达是上游违约，
            // 保留可见信号；广播是 no-op（下游遍历零根 K 线），无害
            ExchangeEventData::HistoryCandles(candles) if candles.is_empty() => {
                tracing::error!("HistoryCandles 事件为空（上游过滤失效），广播作 no-op 处理");
                EventRouting::Broadcast
            }
            _ => match self.routing() {
                Some((exchange, symbol)) => EventRouting::BySymbol { exchange, symbol },
                None => EventRouting::Broadcast,
            },
        }
    }

    /// 获取交易所时间戳
    pub fn exchange_ts(&self) -> Timestamp {
        self.exchange_ts
    }

    /// 获取本地时间戳
    pub fn local_ts(&self) -> Timestamp {
        self.local_ts
    }
}

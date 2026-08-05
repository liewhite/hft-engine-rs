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
    /// 持仓**基线**：本次会话的起始持仓，写入 [`crate::messaging::SymbolState`]。
    ///
    /// # 唯一合法产地：`ManagerActor` 启动期的 REST 查询
    ///
    /// 持仓的维护模型是「一次性基线 + 之后全程由 [`Self::Fill`] 累加」。基线之所以只能来一次，
    /// 是因为持仓快照与实时 Fill 流之间存在竞态：快照可能已含某笔成交，而该成交的 Fill 稍后
    /// 才到；若用快照覆写后再叠加这笔晚到的 Fill，就会**重复计算**。
    ///
    /// 因此：
    /// - 每个 `(exchange, symbol)` 上本事件**只允许出现一次**，第二次到达即为违约
    ///   （[`crate::messaging::SymbolState::apply`] 会打 error 并忽略）
    /// - 交易所适配层**一律不得**发布本事件。私有 WS 的持仓推送（OKX `positions`、
    ///   Hyperliquid `clearinghouseState`、Binance `ACCOUNT_UPDATE.P`）都是**读数**而非基线，
    ///   要用于校验请走 [`Self::PositionReport`]
    PositionBaseline(Position),
    /// 持仓**对账读数**：交易所侧的当前持仓，**只用于校验，绝不写入本地持仓**。
    ///
    /// 由 [`crate::engine::PositionPollingActor`] 周期性 REST 拉取，
    /// 交给 [`crate::engine::PositionReconcileActor`] 与「基线 + Fill 累加」的结果比对。
    ///
    /// 与 [`Self::PositionBaseline`] 同出一个 `fetch_positions()` 调用，但语义、消费方、
    /// 出现次数都不同，故分成两个变体 —— 合成一个再靠「第一条特殊」区分，等于把这条规则
    /// 藏进下游的一个 `if`，上游无从知晓。
    PositionReport(Position),
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
    /// 融券券源读数 (借券费 + 可借量)；由外部数据源经注入入口推入，喂 Strategy::on_borrow_fee
    BorrowFee(BorrowFee),
    /// 汇率读数 (货币对)；由外部数据源经注入入口推入，喂 Strategy::on_exchange_rate
    ExchangeRate(ExchangeRate),
    /// 时钟事件 (用于超时检测等定时任务)
    Clock,
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
            ExchangeEventData::PositionBaseline(pos)
            | ExchangeEventData::PositionReport(pos) => Some(&pos.symbol),
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
            ExchangeEventData::PositionBaseline(pos)
            | ExchangeEventData::PositionReport(pos) => Some(pos.exchange),
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

    /// 获取交易所时间戳
    pub fn exchange_ts(&self) -> Timestamp {
        self.exchange_ts
    }

    /// 获取本地时间戳
    pub fn local_ts(&self) -> Timestamp {
        self.local_ts
    }
}

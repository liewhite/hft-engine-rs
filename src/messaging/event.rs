use crate::domain::{Balance, Exchange, FundingRate, OrderUpdate, Position, Symbol, Timestamp, BBO};

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
    Position(Position),
    OrderUpdate(OrderUpdate),
    Balance(Balance),
    /// 账户净值 (balance + unrealized_pnl)
    Equity { exchange: Exchange, equity: f64 },
    /// 时钟事件 (用于超时检测等定时任务)
    Clock,
}

impl IncomeEvent {
    /// 获取事件关联的 Symbol
    pub fn symbol(&self) -> Option<&Symbol> {
        match &self.data {
            ExchangeEventData::FundingRate(rate) => Some(&rate.symbol),
            ExchangeEventData::BBO(bbo) => Some(&bbo.symbol),
            ExchangeEventData::Position(pos) => Some(&pos.symbol),
            ExchangeEventData::OrderUpdate(update) => Some(&update.symbol),
            ExchangeEventData::Balance(_)
            | ExchangeEventData::Equity { .. }
            | ExchangeEventData::Clock => None,
        }
    }

    /// 获取事件来源交易所
    pub fn exchange(&self) -> Option<Exchange> {
        match &self.data {
            ExchangeEventData::FundingRate(rate) => Some(rate.exchange),
            ExchangeEventData::BBO(bbo) => Some(bbo.exchange),
            ExchangeEventData::Position(pos) => Some(pos.exchange),
            ExchangeEventData::OrderUpdate(update) => Some(update.exchange),
            ExchangeEventData::Balance(bal) => Some(bal.exchange),
            ExchangeEventData::Equity { exchange, .. } => Some(*exchange),
            ExchangeEventData::Clock => None,
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

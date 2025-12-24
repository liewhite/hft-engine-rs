use crate::domain::{Balance, Exchange, FundingRate, OrderUpdate, Position, Symbol, Timestamp, BBO};

/// 统一的交易所事件类型
#[derive(Debug, Clone)]
pub enum ExchangeEvent {
    FundingRateUpdate {
        symbol: Symbol,
        exchange: Exchange,
        rate: FundingRate,
        timestamp: Timestamp,
    },
    BBOUpdate {
        symbol: Symbol,
        exchange: Exchange,
        bbo: BBO,
        timestamp: Timestamp,
    },
    PositionUpdate {
        symbol: Symbol,
        exchange: Exchange,
        position: Position,
        timestamp: Timestamp,
    },
    OrderStatusUpdate {
        symbol: Symbol,
        exchange: Exchange,
        update: OrderUpdate,
        timestamp: Timestamp,
    },
    BalanceUpdate {
        exchange: Exchange,
        balance: Balance,
        timestamp: Timestamp,
    },
}

impl ExchangeEvent {
    /// 获取事件关联的 Symbol (BalanceUpdate 返回 None)
    pub fn symbol(&self) -> Option<&Symbol> {
        match self {
            Self::FundingRateUpdate { symbol, .. } => Some(symbol),
            Self::BBOUpdate { symbol, .. } => Some(symbol),
            Self::PositionUpdate { symbol, .. } => Some(symbol),
            Self::OrderStatusUpdate { symbol, .. } => Some(symbol),
            Self::BalanceUpdate { .. } => None,
        }
    }

    /// 获取事件来源交易所
    pub fn exchange(&self) -> Exchange {
        match self {
            Self::FundingRateUpdate { exchange, .. } => *exchange,
            Self::BBOUpdate { exchange, .. } => *exchange,
            Self::PositionUpdate { exchange, .. } => *exchange,
            Self::OrderStatusUpdate { exchange, .. } => *exchange,
            Self::BalanceUpdate { exchange, .. } => *exchange,
        }
    }

    /// 获取事件时间戳
    pub fn timestamp(&self) -> Timestamp {
        match self {
            Self::FundingRateUpdate { timestamp, .. } => *timestamp,
            Self::BBOUpdate { timestamp, .. } => *timestamp,
            Self::PositionUpdate { timestamp, .. } => *timestamp,
            Self::OrderStatusUpdate { timestamp, .. } => *timestamp,
            Self::BalanceUpdate { timestamp, .. } => *timestamp,
        }
    }
}

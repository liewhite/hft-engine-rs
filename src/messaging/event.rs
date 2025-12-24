use crate::domain::{Balance, Exchange, FundingRate, OrderUpdate, Position, Symbol, BBO};
use std::time::Instant;

/// 统一的 Symbol 事件类型
#[derive(Debug, Clone)]
pub enum SymbolEvent {
    FundingRateUpdate {
        symbol: Symbol,
        exchange: Exchange,
        rate: FundingRate,
        timestamp: Instant,
    },
    BBOUpdate {
        symbol: Symbol,
        exchange: Exchange,
        bbo: BBO,
        timestamp: Instant,
    },
    PositionUpdate {
        symbol: Symbol,
        exchange: Exchange,
        position: Position,
        timestamp: Instant,
    },
    OrderStatusUpdate {
        symbol: Symbol,
        exchange: Exchange,
        update: OrderUpdate,
        timestamp: Instant,
    },
    BalanceUpdate {
        exchange: Exchange,
        balance: Balance,
        timestamp: Instant,
    },
}

impl SymbolEvent {
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
    pub fn timestamp(&self) -> Instant {
        match self {
            Self::FundingRateUpdate { timestamp, .. } => *timestamp,
            Self::BBOUpdate { timestamp, .. } => *timestamp,
            Self::PositionUpdate { timestamp, .. } => *timestamp,
            Self::OrderStatusUpdate { timestamp, .. } => *timestamp,
            Self::BalanceUpdate { timestamp, .. } => *timestamp,
        }
    }
}

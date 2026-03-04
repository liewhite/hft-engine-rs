use crate::domain;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(20))")]
pub enum SignalType {
    #[sea_orm(string_value = "open")]
    Open,
    #[sea_orm(string_value = "close")]
    Close,
    #[sea_orm(string_value = "rebalance")]
    Rebalance,
    #[sea_orm(string_value = "unknown")]
    Unknown,
}

impl SignalType {
    /// 从 comment 前缀提取 signal_type
    pub fn from_comment(comment: &str) -> Self {
        if comment.starts_with("spread_open") {
            Self::Open
        } else if comment.starts_with("spread_close") {
            Self::Close
        } else if comment.starts_with("spread_rebal") {
            Self::Rebalance
        } else {
            Self::Unknown
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(50))")]
pub enum Direction {
    #[sea_orm(string_value = "long_perp_short_spot")]
    LongPerpShortSpot,
    #[sea_orm(string_value = "short_perp_long_spot")]
    ShortPerpLongSpot,
    #[sea_orm(string_value = "unknown")]
    Unknown,
}

impl From<domain::Side> for Direction {
    fn from(side: domain::Side) -> Self {
        match side {
            domain::Side::Long => Self::LongPerpShortSpot,
            domain::Side::Short => Self::ShortPerpLongSpot,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(10))")]
pub enum DbSide {
    #[sea_orm(string_value = "Long")]
    Long,
    #[sea_orm(string_value = "Short")]
    Short,
}

impl From<domain::Side> for DbSide {
    fn from(side: domain::Side) -> Self {
        match side {
            domain::Side::Long => Self::Long,
            domain::Side::Short => Self::Short,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(20))")]
pub enum DbOrderStatus {
    #[sea_orm(string_value = "created")]
    Created,
    #[sea_orm(string_value = "pending")]
    Pending,
    #[sea_orm(string_value = "partially_filled")]
    PartiallyFilled,
    #[sea_orm(string_value = "filled")]
    Filled,
    #[sea_orm(string_value = "cancelled")]
    Cancelled,
    #[sea_orm(string_value = "rejected")]
    Rejected,
    #[sea_orm(string_value = "error")]
    Error,
}

impl From<&domain::OrderStatus> for DbOrderStatus {
    fn from(status: &domain::OrderStatus) -> Self {
        match status {
            domain::OrderStatus::Created => Self::Created,
            domain::OrderStatus::Pending => Self::Pending,
            domain::OrderStatus::PartiallyFilled { .. } => Self::PartiallyFilled,
            domain::OrderStatus::Filled => Self::Filled,
            domain::OrderStatus::Cancelled => Self::Cancelled,
            domain::OrderStatus::Rejected { .. } => Self::Rejected,
            domain::OrderStatus::Error { .. } => Self::Error,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(20))")]
pub enum DbOrderType {
    #[sea_orm(string_value = "limit")]
    Limit,
    #[sea_orm(string_value = "market")]
    Market,
}

impl From<&domain::OrderType> for DbOrderType {
    fn from(order_type: &domain::OrderType) -> Self {
        match order_type {
            domain::OrderType::Limit { .. } => Self::Limit,
            domain::OrderType::Market => Self::Market,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(10))")]
pub enum DbTimeInForce {
    #[sea_orm(string_value = "GTC")]
    Gtc,
    #[sea_orm(string_value = "IOC")]
    Ioc,
    #[sea_orm(string_value = "FOK")]
    Fok,
    #[sea_orm(string_value = "PostOnly")]
    PostOnly,
}

impl From<domain::TimeInForce> for DbTimeInForce {
    fn from(tif: domain::TimeInForce) -> Self {
        match tif {
            domain::TimeInForce::GTC => Self::Gtc,
            domain::TimeInForce::IOC => Self::Ioc,
            domain::TimeInForce::FOK => Self::Fok,
            domain::TimeInForce::PostOnly => Self::PostOnly,
        }
    }
}

impl DbTimeInForce {
    pub fn from_order_type(order_type: &domain::OrderType) -> Self {
        match order_type {
            domain::OrderType::Limit { tif, .. } => (*tif).into(),
            domain::OrderType::Market => Self::Ioc,
        }
    }
}

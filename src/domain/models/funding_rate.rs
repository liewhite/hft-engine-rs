use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Rate, Timestamp};

/// 资金费率
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub rate: Rate,
    pub next_settle_time: Timestamp,
    /// 数据时间戳（毫秒）
    pub timestamp: Timestamp,
}

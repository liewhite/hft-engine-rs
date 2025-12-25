use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Rate, Timestamp};

/// 资金费率
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub rate: Rate,
    pub next_settle_time: Timestamp,
    /// 结算间隔 (小时)，用于日化费率计算
    pub settle_interval_hours: f64,
}

impl FundingRate {
    /// 日化费率 = rate * (24 / settle_interval_hours)
    pub fn daily_rate(&self) -> Rate {
        if self.settle_interval_hours <= 0.0 {
            return 0.0;
        }
        self.rate * (24.0 / self.settle_interval_hours)
    }

    /// 年化费率 = daily_rate * 365
    pub fn annualized_rate(&self) -> Rate {
        self.daily_rate() * 365.0
    }
}

use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Rate, Timestamp};

/// 资金费率
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub rate: Rate,
    pub next_settle_time: Timestamp,
    /// 数据时间戳（毫秒），用于计算基于剩余时间的日化费率
    pub timestamp: Timestamp,
}

impl FundingRate {
    /// 基于剩余时间的日化费率
    ///
    /// 公式：rate * 24 / hours_to_settle
    ///
    /// 例如：
    /// - binance 距离下次资费 5 小时，费率 0.05%，日化 = 0.05% * 24 / 5 = 0.24%
    /// - hyperliquid 距离下次资费 1 小时，费率 0.02%，日化 = 0.02% * 24 / 1 = 0.48%
    pub fn daily_rate(&self) -> Rate {
        if self.timestamp >= self.next_settle_time {
            // 已过结算时间，返回 0（数据过期）
            return 0.0;
        }

        let ms_to_settle = self.next_settle_time - self.timestamp;
        let hours_to_settle = ms_to_settle as f64 / (1000.0 * 60.0 * 60.0);

        if hours_to_settle <= 0.0 {
            return 0.0;
        }

        // 为防止临近结算时日化费率过高，设置最小时间为 0.5 小时
        let effective_hours = hours_to_settle.max(0.5);
        self.rate * (24.0 / effective_hours)
    }
}

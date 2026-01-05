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
    /// 数据时间戳（毫秒），用于计算基于剩余时间的日化费率
    pub timestamp: Timestamp,
}

impl FundingRate {
    /// 日化费率 = rate * (24 / settle_interval_hours)
    ///
    /// 基于固定结算周期计算
    pub fn daily_rate(&self) -> Rate {
        if self.settle_interval_hours <= 0.0 {
            return 0.0;
        }
        self.rate * (24.0 / self.settle_interval_hours)
    }

    /// 基于剩余时间的日化费率
    ///
    /// 公式：rate * 24 / hours_to_settle
    ///
    /// 参数 reference_time: 计算时使用的参考时间戳（通常使用多个交易所中最小的时间戳）
    ///
    /// 例如：
    /// - binance 距离下次资费 5 小时，费率 0.05%，日化 = 0.05% * 24 / 5 = 0.24%
    /// - hyperliquid 距离下次资费 1 小时，费率 0.02%，日化 = 0.02% * 24 / 1 = 0.48%
    pub fn daily_rate_by_time_remaining(&self, reference_time: Timestamp) -> Rate {
        if reference_time >= self.next_settle_time {
            // 已过结算时间，使用固定周期
            return self.daily_rate();
        }

        let ms_to_settle = self.next_settle_time - reference_time;
        let hours_to_settle = ms_to_settle as f64 / (1000.0 * 60.0 * 60.0);

        if hours_to_settle <= 0.0 {
            return self.daily_rate();
        }

        // 为防止临近结算时日化费率过高，设置最小时间为 0.5 小时
        let effective_hours = hours_to_settle.max(0.5);
        self.rate * (24.0 / effective_hours)
    }

    /// 年化费率 = daily_rate * 365
    pub fn annualized_rate(&self) -> Rate {
        self.daily_rate() * 365.0
    }
}

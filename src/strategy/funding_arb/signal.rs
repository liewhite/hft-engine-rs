//! 资金费信号：过去 24h 已结算费率 → 年化 → 开 / 平 / 持有。纯函数，不碰时间源以外的状态。

use crate::domain::{Exchange, SettledFunding, Symbol, Timestamp};

/// 一个窗口内某所某币**全部**已结算费率的快照（由 bin 里的轮询任务拉取后，以
/// `CustomEvent::for_symbol` 投递给策略）。整窗替换，不做增量合并：
/// 每次都是"窗口内的全部事实"，丢一次投递只是数据旧一点，不会累计出错。
#[derive(Debug, Clone)]
pub struct SettledFundingWindow {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 窗口起点（含），= fetched_at - 24h
    pub window_start: Timestamp,
    /// 拉取完成时刻（本地毫秒）
    pub fetched_at: Timestamp,
    pub entries: Vec<SettledFunding>,
}

/// 窗口长度：过去一天（用户规格）
pub const FUNDING_WINDOW_MS: u64 = 24 * 3600 * 1000;

/// 判定"上市满一天"的容差：窗口内第一次结算须落在起点后这么久以内。
/// 取所有周期里最长的 8h + 余量；更晚说明该币上市不足一天，累计费率只覆盖半天会被低估。
const MAX_FIRST_SETTLE_DELAY_MS: u64 = 8 * 3600 * 1000 + 10 * 60 * 1000;

impl SettledFundingWindow {
    /// 年化 = 窗口内费率累加 × 365。数据过旧（`fetched_at` 距 `now` 超过 `max_age_ms`）、
    /// 窗口为空或覆盖不满一天时返回 `None`（未知，不是 0）。
    pub fn apr(&self, now: Timestamp, max_age_ms: u64) -> Option<f64> {
        if now.saturating_sub(self.fetched_at) > max_age_ms {
            return None;
        }
        let first = self.entries.iter().map(|e| e.settle_time).min()?;
        if first.saturating_sub(self.window_start) > MAX_FIRST_SETTLE_DELAY_MS {
            return None;
        }
        let sum: f64 = self
            .entries
            .iter()
            .filter(|e| e.settle_time >= self.window_start)
            .map(|e| e.rate)
            .sum();
        Some(sum * 365.0)
    }
}

/// 持仓方向：做空费率高的一所、做多费率低的一所
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 币安空、HL 多（币安年化 > HL 年化）
    ShortBinanceLongHl,
    /// 币安多、HL 空（HL 年化 > 币安年化）
    LongBinanceShortHl,
}

impl Direction {
    /// 从币安持仓方向推出（HL 恰好相反）；空仓为 None
    pub fn from_binance_position(bn_size: f64) -> Option<Self> {
        if bn_size < 0.0 {
            Some(Direction::ShortBinanceLongHl)
        } else if bn_size > 0.0 {
            Some(Direction::LongBinanceShortHl)
        } else {
            None
        }
    }

    /// 按年化高低推出该持有的方向；相等时无方向
    fn favoured(bn_apr: f64, hl_apr: f64) -> Option<Self> {
        if bn_apr > hl_apr {
            Some(Direction::ShortBinanceLongHl)
        } else if hl_apr > bn_apr {
            Some(Direction::LongBinanceShortHl)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingAction {
    /// 按该方向开仓 / 继续加仓（受杠杆限制）
    Open(Direction),
    /// 平掉现有仓位
    Close,
    /// 不动
    Hold,
}

/// 用户规格：
/// - 年化差 ≥ 门槛 → 按费率方向开（已有同向仓位则继续加）；
/// - 已有仓位而过去一天年化**反转**（费率高的一所换了边）→ 平仓；
/// - 其余持有。
pub fn decide(bn_apr: f64, hl_apr: f64, held: Option<Direction>, open_threshold_apr: f64) -> FundingAction {
    let favoured = Direction::favoured(bn_apr, hl_apr);
    match held {
        Some(h) if favoured.is_some_and(|f| f != h) => FundingAction::Close,
        _ => match favoured {
            Some(f) if (bn_apr - hl_apr).abs() >= open_threshold_apr => FundingAction::Open(f),
            _ => FundingAction::Hold,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3600 * 1000;

    fn window(ex: Exchange, start: u64, rates: &[(u64, f64)]) -> SettledFundingWindow {
        SettledFundingWindow {
            exchange: ex,
            symbol: "BTC".into(),
            window_start: start,
            fetched_at: start + 24 * H,
            entries: rates
                .iter()
                .map(|&(t, rate)| SettledFunding { exchange: ex, symbol: "BTC".into(), rate, settle_time: t })
                .collect(),
        }
    }

    #[test]
    fn apr_sums_settlements_regardless_of_interval() {
        // 用户例子：HL 每小时 1 份 × 24，币安每 8 小时 3 份 × 3 —— 都是一天 24 份
        let hl = window(Exchange::Hyperliquid, 0, &(1..=24).map(|i| (i * H, 1e-5)).collect::<Vec<_>>());
        let bn = window(Exchange::Binance, 0, &[(8 * H, 8e-5), (16 * H, 8e-5), (24 * H, 8e-5)]);
        let now = 24 * H;
        assert!((hl.apr(now, H).unwrap() - 24.0 * 1e-5 * 365.0).abs() < 1e-12);
        assert!((bn.apr(now, H).unwrap() - 24.0 * 1e-5 * 365.0).abs() < 1e-12);
    }

    #[test]
    fn apr_unknown_when_stale_empty_or_listed_less_than_a_day() {
        let w = window(Exchange::Binance, 0, &[(8 * H, 1e-4)]);
        assert!(w.apr(24 * H + 2 * H, H).is_none(), "stale");
        assert!(window(Exchange::Binance, 0, &[]).apr(24 * H, H).is_none(), "empty");
        assert!(window(Exchange::Binance, 0, &[(12 * H, 1e-4)]).apr(24 * H, H).is_none(), "half day");
    }

    #[test]
    fn decide_follows_user_spec() {
        use Direction::*;
        use FundingAction::*;
        // 无仓：差 ≥ 门槛才开，空费率高的一所
        assert_eq!(decide(0.8, 0.1, None, 0.5), Open(ShortBinanceLongHl));
        assert_eq!(decide(-0.3, 0.3, None, 0.5), Open(LongBinanceShortHl));
        assert_eq!(decide(0.4, 0.0, None, 0.5), Hold);
        // 有仓同向：差大继续加，差小持有
        assert_eq!(decide(0.8, 0.1, Some(ShortBinanceLongHl), 0.5), Open(ShortBinanceLongHl));
        assert_eq!(decide(0.2, 0.1, Some(ShortBinanceLongHl), 0.5), Hold);
        // 反转即平，不管差多小
        assert_eq!(decide(0.1, 0.11, Some(ShortBinanceLongHl), 0.5), Close);
        // 相等不算反转
        assert_eq!(decide(0.1, 0.1, Some(ShortBinanceLongHl), 0.5), Hold);
    }
}

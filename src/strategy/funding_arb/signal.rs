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

/// 一对所上的一次对冲操作：在 `sell` 卖、在 `buy` 买（数量相同）。
/// 开仓时 `sell` 是费率高的一所；平仓时 `sell` 是持多仓的一所。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegPair {
    pub sell: Exchange,
    pub buy: Exchange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingAction {
    /// 在这对所之间开仓 / 继续加仓（受杠杆限制）；可能顺带削减已有的腿
    Open(LegPair),
    /// 在这对所之间平掉相反仓位（只减仓）
    Close(LegPair),
    /// 不动
    Hold,
}

/// 用户规格（#673）：
/// - **平仓优先**：任意一对「一所多、一所空」的仓位，若空头所年化 − 多头所年化 < 平仓门槛
///   （过去一天这对仓位的收益率低于门槛，含反转），在这两所之间平仓；几对同时满足取收益率最低的。
///   只看这两所的年化，第三所数据缺失不影响。
/// - 否则在年化最高与最低的两所之间（差 ≥ 开仓门槛）开仓：高的卖、低的买。须全部所的年化都已知，
///   否则「差最大的两所」无从判断。
/// - 其余持有。
///
/// `aprs`：各所年化，`None` = 未知；`positions`：各所持仓（只传价值够一笔最小单的，正多负空）。
/// 两者的顺序决定同分时的取舍，调用方保持固定顺序即可。
///
/// 平仓优先不会与开仓来回打架：开仓选的是全局最高 / 最低两所，新形成的多空对收益率
/// 至少等于原有某对的收益率（原有各对都已 ≥ 平仓门槛，否则先平），不会立刻满足平仓条件。
/// 同理，开仓要在某所削减已有的腿（在持多的最高所卖、或在持空的最低所买）时，那条腿所在的仓位对
/// 收益已 ≤ 0，必先被平仓条件接住；用户允许开仓削减已有腿，实际只会削到不足一笔最小单的零头。
pub fn decide(
    aprs: &[(Exchange, Option<f64>)],
    positions: &[(Exchange, f64)],
    open_threshold_apr: f64,
    close_threshold_apr: f64,
) -> FundingAction {
    let apr = |ex: Exchange| aprs.iter().find(|(e, _)| *e == ex).and_then(|(_, a)| *a);

    let mut close: Option<(f64, LegPair)> = None;
    for &(long_ex, _) in positions.iter().filter(|(_, p)| *p > 0.0) {
        for &(short_ex, _) in positions.iter().filter(|(_, p)| *p < 0.0) {
            let (Some(long_apr), Some(short_apr)) = (apr(long_ex), apr(short_ex)) else {
                continue;
            };
            let earn = short_apr - long_apr;
            if earn < close_threshold_apr && close.is_none_or(|(best, _)| earn < best) {
                close = Some((earn, LegPair { sell: long_ex, buy: short_ex }));
            }
        }
    }
    if let Some((_, pair)) = close {
        return FundingAction::Close(pair);
    }

    let mut known = Vec::with_capacity(aprs.len());
    for &(ex, a) in aprs {
        match a {
            Some(a) => known.push((ex, a)),
            None => return FundingAction::Hold,
        }
    }
    // 同分取靠前的所（顺序固定，结果确定）
    let high = known.iter().copied().reduce(|best, x| if x.1 > best.1 { x } else { best });
    let low = known.iter().copied().reduce(|best, x| if x.1 < best.1 { x } else { best });
    match (high, low) {
        (Some((h, ha)), Some((l, la))) if h != l && ha - la >= open_threshold_apr => {
            FundingAction::Open(LegPair { sell: h, buy: l })
        }
        _ => FundingAction::Hold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3600 * 1000;
    const BN: Exchange = Exchange::Binance;
    const HL: Exchange = Exchange::Hyperliquid;
    const OKX: Exchange = Exchange::OKX;

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
        let hl = window(HL, 0, &(1..=24).map(|i| (i * H, 1e-5)).collect::<Vec<_>>());
        let bn = window(BN, 0, &[(8 * H, 8e-5), (16 * H, 8e-5), (24 * H, 8e-5)]);
        let now = 24 * H;
        assert!((hl.apr(now, H).unwrap() - 24.0 * 1e-5 * 365.0).abs() < 1e-12);
        assert!((bn.apr(now, H).unwrap() - 24.0 * 1e-5 * 365.0).abs() < 1e-12);
    }

    #[test]
    fn apr_unknown_when_stale_empty_or_listed_less_than_a_day() {
        let w = window(BN, 0, &[(8 * H, 1e-4)]);
        assert!(w.apr(24 * H + 2 * H, H).is_none(), "stale");
        assert!(window(BN, 0, &[]).apr(24 * H, H).is_none(), "empty");
        assert!(window(BN, 0, &[(12 * H, 1e-4)]).apr(24 * H, H).is_none(), "half day");
    }

    fn aprs(bn: f64, hl: f64, okx: f64) -> Vec<(Exchange, Option<f64>)> {
        vec![(BN, Some(bn)), (HL, Some(hl)), (OKX, Some(okx))]
    }

    use FundingAction::*;

    #[test]
    fn open_between_highest_and_lowest() {
        // 无仓：差最大的两所，高卖低买；差不够不开
        assert_eq!(decide(&aprs(0.8, 0.1, 0.3), &[], 0.5, 0.1), Open(LegPair { sell: BN, buy: HL }));
        assert_eq!(decide(&aprs(0.0, -0.2, 0.4), &[], 0.5, 0.1), Open(LegPair { sell: OKX, buy: HL }));
        assert_eq!(decide(&aprs(0.4, 0.0, 0.2), &[], 0.5, 0.1), Hold);
        // 所对随费率变化：已有 BN空/HL多，今天 OKX 最高 → 在 OKX×HL 继续开（不平 BN×HL：它仍赚 ≥ 10%）
        let pos = [(BN, -1.0), (HL, 1.0)];
        assert_eq!(decide(&aprs(0.5, 0.0, 0.9), &pos, 0.5, 0.1), Open(LegPair { sell: OKX, buy: HL }));
        // BN 多 / OKX 空，今天 BN 最高：开仓会在 BN 卖（削减多腿），但这对仓收益
        // OKX−BN = 0.0−0.9 < 10%，平仓优先先平
        let pos = [(BN, 1.0), (OKX, -1.0)];
        assert_eq!(decide(&aprs(0.9, 0.3, 0.0), &pos, 0.5, 0.1), Close(LegPair { sell: BN, buy: OKX }));
    }

    #[test]
    fn close_when_pair_earns_below_threshold() {
        // BN 空 HL 多：收益 = BN − HL；低于 10% 平（含反转），平仓优先于开仓
        let pos = [(BN, -1.0), (HL, 1.0)];
        assert_eq!(decide(&aprs(0.15, 0.0, 0.0), &pos, 0.5, 0.1), Hold);
        assert_eq!(decide(&aprs(0.05, 0.0, 0.0), &pos, 0.5, 0.1), Close(LegPair { sell: HL, buy: BN }));
        assert_eq!(decide(&aprs(-0.3, 0.0, 0.9), &pos, 0.5, 0.1), Close(LegPair { sell: HL, buy: BN }));
        // 第三所不动；几对都满足时取收益最低的一对
        let pos = [(BN, -2.0), (HL, 1.0), (OKX, 1.0)];
        assert_eq!(decide(&aprs(0.0, 0.05, -0.02), &pos, 0.5, 0.1), Close(LegPair { sell: HL, buy: BN }));
    }

    #[test]
    fn unknown_apr_blocks_open_but_not_unrelated_close() {
        let mut a = aprs(0.8, 0.0, 0.0);
        a[2].1 = None;
        // OKX 未知：不知道谁差最大，不开
        assert_eq!(decide(&a, &[], 0.5, 0.1), Hold);
        // BN×HL 这对的平仓只看这两所
        let pos = [(BN, 1.0), (HL, -1.0)];
        assert_eq!(decide(&a, &pos, 0.5, 0.1), Close(LegPair { sell: BN, buy: HL }));
    }
}

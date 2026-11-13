//! 价格精度规则：把价格取到交易所接受的合法值（精确十进制，见 [`crate::domain::Price`]）。
//!
//! 不同交易所规则不同：
//! - Binance / OKX / IBKR：固定步长（tick），见 [`PriceStep`]
//! - Hyperliquid：5 位有效数字且小数位不超过 `6 - szDecimals`，见 [`HlSigFigs`]
//!
//! 下单字符串一律由 [`crate::domain::to_wire`] 写出（无尾零、无科学计数法），规则里不再各自拼字符串。

use crate::domain::Price;
use rust_decimal::{Decimal, RoundingStrategy};
use std::fmt::Debug;

/// 价格精度规则
pub trait PriceRule: Send + Sync + Debug {
    /// 取到**最近**的合法价格（四舍五入，中点远离 0）
    fn round(&self, price: Price) -> Price;
}

/// 固定步长（Binance / OKX / IBKR）：合法价格是 `step` 的整数倍
#[derive(Debug, Clone)]
pub struct PriceStep {
    step: Decimal,
}

impl PriceStep {
    /// `step` 必须为正：交易所规格里的 tick 不可能是 0 或负数，是了就是解析错，当场报错
    pub fn new(step: Decimal) -> Self {
        assert!(step > Decimal::ZERO, "价格步长必须为正：{step}");
        Self { step }
    }
}

impl PriceRule for PriceStep {
    fn round(&self, price: Price) -> Price {
        (price / self.step).round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero) * self.step
    }
}

/// Hyperliquid 永续：至多 5 位有效数字，且小数位不超过 `6 - szDecimals`；整数价格总是合法。
#[derive(Debug, Clone)]
pub struct HlSigFigs {
    sz_decimals: u32,
}

impl HlSigFigs {
    pub fn new(sz_decimals: u32) -> Self {
        Self { sz_decimals }
    }
}

impl PriceRule for HlSigFigs {
    fn round(&self, price: Price) -> Price {
        if price <= Decimal::ZERO || price.fract().is_zero() {
            return price;
        }
        // 首位有效数字的十进制指数：12345.6 → 4，0.000251 → -4。
        // normalize 后 price = mantissa × 10^-scale，mantissa 有 n 位，首位指数 = n - 1 - scale
        let p = price.normalize();
        let digits = p.mantissa().unsigned_abs().to_string().len() as i64;
        let lead_exp = digits - 1 - p.scale() as i64;
        let sig_fig_decimals = (4 - lead_exp).max(0) as u32;
        let max_decimals = 6_u32.saturating_sub(self.sz_decimals);
        price.round_dp_with_strategy(sig_fig_decimals.min(max_decimals), RoundingStrategy::MidpointAwayFromZero)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn s(rule: &dyn PriceRule, p: Decimal) -> String {
        rule.round(p).normalize().to_string()
    }

    #[test]
    fn step_rounds_to_nearest_tick() {
        let f = PriceStep::new(dec!(0.01));
        assert_eq!(s(&f, dec!(100.123)), "100.12");
        assert_eq!(s(&f, dec!(100.126)), "100.13");
        assert_eq!(s(&f, dec!(100.125)), "100.13", "中点远离 0，不是银行家舍入");
        assert_eq!(s(&f, dec!(100)), "100");
        assert_eq!(s(&f, dec!(10000000000)), "10000000000");
        let f1 = PriceStep::new(dec!(1));
        assert_eq!(s(&f1, dec!(4870)), "4870", "整数末尾的 0 不能删");
        assert_eq!(s(&f1, dec!(4869.6)), "4870");
        let q = PriceStep::new(dec!(0.25));
        assert_eq!(s(&q, dec!(100.25)), "100.25");
        assert_eq!(s(&q, dec!(100.3)), "100.25");
        assert_eq!(s(&PriceStep::new(dec!(0.005)), dec!(1.2345)), "1.235");
        assert_eq!(s(&PriceStep::new(dec!(0.00001)), dec!(0.0000123456)), "0.00001");
    }

    #[test]
    fn step_rounding_is_idempotent() {
        let f = PriceStep::new(dec!(0.1));
        for k in 0..10_000i64 {
            let p = Decimal::new(k, 1); // k × 0.1
            assert_eq!(f.round(p), p);
        }
    }

    #[test]
    fn hl_significant_figures() {
        let f0 = HlSigFigs::new(0);
        assert_eq!(s(&f0, dec!(0.000251)), "0.000251");
        assert_eq!(s(&f0, dec!(0.00012345)), "0.000123", "受 6 位小数限制");
        let f2 = HlSigFigs::new(2);
        assert_eq!(s(&f2, dec!(0.12345)), "0.1235", "受 max_decimals=4 限制");
        assert_eq!(s(&f2, dec!(1234.5)), "1234.5");
        assert_eq!(s(&f2, dec!(1234.56)), "1234.6");
        assert_eq!(s(&f2, dec!(100000)), "100000");
        assert_eq!(s(&f2, dec!(123456789)), "123456789", "整数价格总是合法");
        let f5 = HlSigFigs::new(5);
        assert_eq!(s(&f5, dec!(106290.4)), "106290");
        assert_eq!(s(&f5, dec!(99999.6)), "100000");
        assert_eq!(s(&f5, dec!(70010.2)), "70010");
        assert_eq!(s(&f5, dec!(12345.6)), "12346");
        let f1 = HlSigFigs::new(1);
        assert_eq!(s(&f1, dec!(0.123456)), "0.12346");
        assert_eq!(s(&f1, dec!(1.000001)), "1");
    }
}

//! 价格精度规则：把价格取到交易所接受的合法值（精确十进制，见 [`crate::domain::Price`]）。
//!
//! 规则是各所的市场知识：通用的固定步长（tick）在这里，见 [`PriceStep`]；不是固定步长的所在自己的
//! 适配层实现 [`PriceRule`]（如 Hyperliquid 的有效数字规则，见 `exchange::hyperliquid::markets`）。
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

/// 固定步长：合法价格是 `step` 的整数倍
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
        (price / self.step).round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
            * self.step
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
        assert_eq!(
            s(&PriceStep::new(dec!(0.00001)), dec!(0.0000123456)),
            "0.00001"
        );
    }

    #[test]
    fn step_rounding_is_idempotent() {
        let f = PriceStep::new(dec!(0.1));
        for k in 0..10_000i64 {
            let p = Decimal::new(k, 1); // k × 0.1
            assert_eq!(f.round(p), p);
        }
    }
}

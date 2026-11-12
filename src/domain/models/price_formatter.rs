//! 价格格式化器
//!
//! 不同交易所有不同的价格精度规则：
//! - Binance/OKX: 固定步长 (price_step)
//! - Hyperliquid: 有效数字模式

use std::fmt::Debug;

/// 价格格式化 trait
pub trait PriceFormatter: Send + Sync + Debug {
    /// 格式化价格为字符串 (用于 API 请求)
    fn format(&self, price: f64) -> String;
}

// ============================================================================
// StepFormatter - Binance/OKX
// ============================================================================

/// 固定步长价格格式化器 (Binance/OKX)
#[derive(Debug, Clone)]
pub struct StepFormatter {
    /// 价格步长
    step: f64,
    /// 小数位数 (从 step 计算)
    decimals: usize,
}

impl StepFormatter {
    pub fn new(step: f64) -> Self {
        let decimals = Self::step_to_decimals(step);
        Self { step, decimals }
    }

    /// 步长的小数位数 = 步长十进制写法的小数位（0.25 → 2、0.001 → 3、5 → 0）。
    ///
    /// 不用 `-log10(step).floor()`：那只对 10 的幂次对，0.25 会算成 1 位，100.25 被写成 "100.3"。
    /// f64 的 `to_string` 是最短往返写法（0.1 → "0.1"），据此数小数位是精确的。
    fn step_to_decimals(step: f64) -> usize {
        let text = step.to_string();
        text.split_once('.').map_or(0, |(_, frac)| frac.len())
    }

    fn round_to_step(value: f64, step: f64) -> f64 {
        (value / step).round() * step
    }
}

impl PriceFormatter for StepFormatter {
    fn format(&self, price: f64) -> String {
        let rounded = Self::round_to_step(price, self.step);
        trim_fraction_zeros(format!("{:.precision$}", rounded, precision = self.decimals))
    }
}

/// 删小数部分的尾随零（及多余的小数点）。没有小数点时原样返回：纯整数末尾的 0 是数值的一部分，
/// 删了会把 4870 变成 487（两个格式化器都曾这样错过，故收成这一处）。
fn trim_fraction_zeros(s: String) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

// ============================================================================
// SignificantFiguresFormatter - Hyperliquid
// ============================================================================

/// 有效数字价格格式化器 (Hyperliquid)
///
/// 规则：
/// - max_decimals = 6 - sz_decimals
/// - 保持 5 位有效数字
/// - 取两者更严格的限制
#[derive(Debug, Clone)]
pub struct SignificantFiguresFormatter {
    /// 数量小数位数
    sz_decimals: u32,
}

impl SignificantFiguresFormatter {
    pub fn new(sz_decimals: u32) -> Self {
        Self { sz_decimals }
    }

    /// 计算价格的小数位数
    fn calculate_decimals(&self, price: f64) -> u32 {
        let max_decimal_places = 6_u32.saturating_sub(self.sz_decimals);

        // 基于 5 位有效数字计算小数位限制
        let sig_fig_decimals = if price >= 1.0 {
            let int_digits = price.log10().floor() as u32 + 1;
            5_u32.saturating_sub(int_digits)
        } else {
            let leading_zeros = (-price.log10().ceil()) as u32;
            5 + leading_zeros
        };

        // 取更严格的限制
        max_decimal_places.min(sig_fig_decimals)
    }
}

impl PriceFormatter for SignificantFiguresFormatter {
    fn format(&self, price: f64) -> String {
        if price <= 0.0 || !price.is_finite() {
            return "0".to_string();
        }

        // 整数价格直接返回
        if price.fract() == 0.0 {
            return format!("{}", price as u64);
        }

        let decimals = self.calculate_decimals(price);
        let multiplier = 10_f64.powi(decimals as i32);
        let rounded = (price * multiplier).round() / multiplier;

        trim_fraction_zeros(format!("{:.precision$}", rounded, precision = decimals as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_step_formatter() {
        let f = StepFormatter::new(0.01);
        assert_eq!(f.format(100.123), "100.12");
        assert_eq!(f.format(100.126), "100.13");
        assert_eq!(f.format(100_f64), "100");
        assert_eq!(f.format(10000000000_f64), "10000000000");
        // 步长 ≥ 1 时整数末尾的 0 不得删（曾把 4870 格式化成 "487"）
        let f1 = StepFormatter::new(1.0);
        assert_eq!(f1.format(4870.0), "4870");
        assert_eq!(f1.format(4869.6), "4870");
        // 步长不是 10 的幂次：小数位按步长写法算（log10 推法曾把 0.25 算成 1 位）
        let q = StepFormatter::new(0.25);
        assert_eq!(q.format(100.25), "100.25");
        assert_eq!(q.format(100.3), "100.25");
        assert_eq!(StepFormatter::new(0.005).format(1.2345), "1.235");
    }

    #[test]
    fn test_significant_figures_formatter() {
        // Hyperliquid 规则:
        // - 最多 5 位有效数字
        // - 小数位限制: 6 - sz_decimals (永续)
        // - 整数价格总是允许

        // sz_decimals=0, max_decimals=6
        let f0 = SignificantFiguresFormatter::new(0);
        assert_eq!(f0.format(0.000251), "0.000251"); // 6 位小数, 3 位有效数字
        assert_eq!(f0.format(0.00012345), "0.000123"); // 受 5 位有效数字限制

        // sz_decimals=2, max_decimals=4
        let f2 = SignificantFiguresFormatter::new(2);
        assert_eq!(f2.format(0.12345), "0.1235"); // 受 max_decimals=4 限制

        // 取整到整数位时不得删整数末尾的 0（曾把 106290.4 格式化成 "10629"）
        let f5 = SignificantFiguresFormatter::new(5);
        assert_eq!(f5.format(106290.4), "106290");
        assert_eq!(f5.format(99999.6), "100000");
        assert_eq!(f5.format(70010.2), "70010");
        assert_eq!(f5.format(12345.6), "12346");
        assert_eq!(f2.format(1234.5), "1234.5"); // 5 位有效数字

        // 整数价格总是允许
        assert_eq!(f2.format(100000.0), "100000");
        assert_eq!(f2.format(123456789.0), "123456789");
    }
}

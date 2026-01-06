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

    fn step_to_decimals(step: f64) -> usize {
        if step >= 1.0 {
            return 0;
        }
        // 使用 log10 计算小数位数
        let decimals = -step.log10().floor() as usize;
        decimals
    }

    fn round_to_step(value: f64, step: f64) -> f64 {
        (value / step).round() * step
    }
}

impl PriceFormatter for StepFormatter {
    fn format(&self, price: f64) -> String {
        let rounded = Self::round_to_step(price, self.step);
        let s = format!("{:.precision$}", rounded, precision = self.decimals);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
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

        // 格式化并移除尾随零
        let s = format!("{:.precision$}", rounded, precision = decimals as usize);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
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
        assert_eq!(f.format(100.0), "100");
    }

    #[test]
    fn test_significant_figures_formatter() {
        let f = SignificantFiguresFormatter::new(2); // sz_decimals=2, max_decimals=4

        // BTC ~100000, 5 sig figs -> 0 decimals for integer part 6 digits
        assert_eq!(f.format(100000.0), "100000");

        // 小价格
        assert_eq!(f.format(0.12345), "0.1235"); // 5 sig figs, but max 4 decimals
    }
}

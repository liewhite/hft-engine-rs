use crate::domain::Symbol;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct MacdGridConfig {
    /// 交易品种 (e.g., "ETH")
    pub symbol: Symbol,
    /// 每次网格订单金额 (USDT)
    pub order_usd_value: f64,
    /// 强趋势最大持仓 (USDT)
    pub max_position_usd: f64,
    /// 弱趋势最大持仓 (USDT)
    pub weak_position_usd: f64,
    /// IOC 订单滑点 (e.g., 0.001 = 0.1%)
    pub ioc_slippage: f64,
    /// 订单超时 (ms)
    pub order_timeout_ms: u64,
    /// 网格最小间距 (绝对价格)，防止 ATR 过低时网格过密
    pub min_spacing: f64,
    /// 激进侧间距因子 (spacing = ATR * factor)
    /// 强多头的买入间距 / 强空头的卖出间距
    #[serde(default = "default_aggressive")]
    pub aggressive_spacing_factor: f64,
    /// 保守侧间距因子 (spacing = ATR * factor)
    /// 强多头的卖出间距 / 强空头的买入间距 / 弱趋势两侧
    #[serde(default = "default_conservative")]
    pub conservative_spacing_factor: f64,
    /// 超买超卖敏感度权重
    /// deviation = (price - slow_ema) / ATR, ob_factor = clamp(1 ± deviation * ob_weight, 0.5, 3.0)
    #[serde(default = "default_ob_weight")]
    pub ob_weight: f64,
    /// 仓位深度对开仓间距的影响权重
    /// pos_factor = 1 + (pos_usd / max_pos_usd) * pos_weight
    /// 取值范围 [1, 1+pos_weight]，与 ob_factor 尺度对等
    #[serde(default = "default_pos_weight")]
    pub pos_weight: f64,
}

fn default_aggressive() -> f64 {
    0.5
}
fn default_conservative() -> f64 {
    1.0
}
fn default_ob_weight() -> f64 {
    0.5
}
fn default_pos_weight() -> f64 {
    2.0
}

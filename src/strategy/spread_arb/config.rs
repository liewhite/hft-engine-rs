use serde::Deserialize;

/// Spread 套利策略配置 (IBKR 股票 vs Hyperliquid 永续)
#[derive(Debug, Clone, Deserialize)]
pub struct SpreadArbConfig {
    /// 参与套利的 symbol 列表
    pub symbols: Vec<String>,
    /// 开仓阈值 (e.g., 0.003 = 0.3%)
    /// spread > open_threshold 时开仓
    pub open_threshold: f64,
    /// 平仓阈值 (e.g., 0.001 = 0.1%)
    /// spread < close_threshold 时平仓
    pub close_threshold: f64,
    /// 单笔下单金额 (USD)
    pub order_usd_value: f64,
    /// 最大杠杆率 (仓位价值 / equity)
    pub max_leverage: f64,
    /// IOC 滑点 (e.g., 0.001 = 0.1%)
    pub ioc_slippage: f64,
    /// 订单超时 (ms)
    pub order_timeout_ms: u64,
}

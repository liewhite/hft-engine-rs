use serde::Deserialize;

/// BBO 新鲜度默认阈值 (ms)
fn default_bbo_staleness_ms() -> u64 {
    1000
}

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
    /// BBO 新鲜度阈值 (ms)，超过此值视为过期
    #[serde(default = "default_bbo_staleness_ms")]
    pub bbo_staleness_ms: u64,
}

impl SpreadArbConfig {
    /// 启动时校验配置参数合法性，不合法则 panic (fail-fast)
    pub fn validate(&self) {
        assert!(
            self.open_threshold > self.close_threshold,
            "SpreadArbConfig: open_threshold ({}) must be > close_threshold ({})",
            self.open_threshold,
            self.close_threshold,
        );
        assert!(
            self.ioc_slippage >= 0.0,
            "SpreadArbConfig: ioc_slippage ({}) must be >= 0.0",
            self.ioc_slippage,
        );
        assert!(
            self.max_leverage > 0.0,
            "SpreadArbConfig: max_leverage ({}) must be > 0.0",
            self.max_leverage,
        );
        assert!(
            self.order_usd_value > 0.0,
            "SpreadArbConfig: order_usd_value ({}) must be > 0.0",
            self.order_usd_value,
        );
        assert!(
            self.order_timeout_ms > 0,
            "SpreadArbConfig: order_timeout_ms must be > 0",
        );
        assert!(
            self.bbo_staleness_ms > 0,
            "SpreadArbConfig: bbo_staleness_ms must be > 0",
        );
    }
}

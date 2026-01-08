use serde::Deserialize;

/// 跨所价差套利策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct FundingArbConfig {
    /// EMA 周期（表示最近多少笔 BBO 更新的均价）
    pub ema_period: usize,
    /// 开仓 deviation 阈值
    /// - max_bid_deviation + max_ask_deviation > deviation_threshold 时开仓
    pub deviation_threshold: f64,
    /// 单笔下单金额 (USDT)，开平仓均按此金额计算数量
    pub max_notional: f64,
    /// 最小下单金额 (USDT)，低于此金额的订单将被放弃
    pub min_notional: f64,
    /// 订单超时时间 (毫秒)
    pub order_timeout_ms: u64,
    /// 敞口比例限制（敞口/较小仓位）
    /// - 超过此比例时禁止开仓
    /// - 配合 max_exposure_value 触发 rebalance
    pub max_exposure_ratio: f64,
    /// 敞口价值限制 (USDT)
    /// - 需同时超过 max_exposure_ratio 和 max_exposure_value 才触发 rebalance
    /// - 避免基础仓位小时频繁 rebalance
    pub max_exposure_value: f64,
    /// 单边仓位占账户 equity 的最大比例
    /// - 任一交易所的仓位价值 / equity 超过此比例时禁止开仓
    /// - 不影响平仓和 rebalance
    pub max_position_ratio: f64,
}

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
    pub max_symbol_leverage: f64,
    /// 账户级别最大杠杆率 (account_notional / equity)
    /// - 任一交易所超过此阈值，且订单方向与现有仓位方向相同时，禁止开仓
    /// - 用于控制账户整体风险
    pub max_account_leverage: f64,
    /// IOC 订单滑点（用限价单 IOC 模拟市价单）
    /// - 例如 0.001 表示 0.1%
    /// - 做多时 ask + slippage，做空时 bid - slippage
    pub ioc_slippage: f64,
    /// 杠杆率-阈值衰减系数
    /// - 控制 symbol 杠杆率对开仓阈值的影响
    /// - 例如 2.0 表示杠杆率每上升 10%，阈值降低 20%
    /// - 计算公式: effective_threshold = deviation_threshold * max(0, 1 - max_leverage * decay)
    pub leverage_threshold_decay: f64,
}

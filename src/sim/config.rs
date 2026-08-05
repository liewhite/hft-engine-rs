use crate::domain::Rate;

/// 虚拟柜台的延迟与初始资金配置 (与 ox-demo `SimConfig` 同构)。
///
/// 延迟在回测引擎中建模为"入队到 now + 延迟"，撮合本身不依赖墙钟。
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(default)]
pub struct SimConfig {
    /// 交易所 -> 策略 的单向延迟 (行情/订单回报/成交回流)
    pub exchange_to_strategy_delay_ms: u64,
    /// 下单 -> 交易所 的单向延迟 (下单/撤单到达撮合)
    pub order_to_exchange_delay_ms: u64,
    /// 初始账户现金 (USDT)
    pub initial_balance_usdt: f64,
    /// maker 手续费率 (resting 单被越价成交)，0.0002 = 0.02%。默认 0 = 不计费
    pub maker_fee_rate: Rate,
    /// taker 手续费率 (到达即吃单成交)，0.0005 = 0.05%。默认 0 = 不计费
    pub taker_fee_rate: Rate,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            exchange_to_strategy_delay_ms: 50,
            order_to_exchange_delay_ms: 30,
            initial_balance_usdt: 10_000.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }
}

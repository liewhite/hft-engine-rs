use crate::domain::models::Exchange;
use crate::domain::types::Timestamp;

/// 汇率读数（一个货币对）
///
/// 由数据源 (如 IBKR forex snapshot) 周期推送，作为事件喂进策略
/// (策略在 `on_event` 里消费)。`rate` = 1 单位 base 兑多少 quote (如 USD/KRW ≈ 1380)。
#[derive(Debug, Clone)]
pub struct ExchangeRate {
    pub exchange: Exchange,
    /// 基准货币 (如 "USD")
    pub base: String,
    /// 计价货币 (如 "KRW")
    pub quote: String,
    /// 1 base = rate × quote
    pub rate: f64,
    pub timestamp: Timestamp,
}

use crate::domain::models::Exchange;
use crate::domain::types::Timestamp;

/// 资费事件
///
/// 由交易所推送的账户资费结算结果。
/// - `amount > 0`：账户收到资费
/// - `amount < 0`：账户支付资费
#[derive(Debug, Clone)]
pub struct FundingFee {
    pub exchange: Exchange,
    /// 结算币种（如 "USDT"）
    pub asset: String,
    /// 本次结算的资费金额（含正负号）
    pub amount: f64,
    pub timestamp: Timestamp,
}

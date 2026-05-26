use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::Timestamp;

/// 资费结算事件（单笔）
///
/// 跨交易所统一约定：
/// - `amount > 0`：账户收到资费
/// - `amount < 0`：账户支付资费
///
/// `tran_id` 是交易所返回的本笔结算唯一 ID，下游凭此去重。
#[derive(Debug, Clone)]
pub struct FundingFee {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 结算币种（如 "USDT"）
    pub asset: String,
    pub amount: f64,
    pub timestamp: Timestamp,
    /// 交易所唯一结算 ID（供下游去重）
    pub tran_id: u64,
}

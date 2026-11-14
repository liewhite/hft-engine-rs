use crate::domain::models::{Exchange, Money, Symbol};
use crate::domain::types::Timestamp;

/// 资费结算事件（单笔）
///
/// 跨交易所统一约定：
/// - `money.amount > 0`：账户收到资费
/// - `money.amount < 0`：账户支付资费
///
/// `tran_id` 是交易所返回的本笔结算唯一 ID，下游凭此去重。
#[derive(Debug, Clone)]
pub struct FundingFee {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 金额与结算币种（交易所原样，如 USDT、USDC）
    pub money: Money,
    pub timestamp: Timestamp,
    /// 交易所唯一结算 ID（供下游去重）
    pub tran_id: u64,
}

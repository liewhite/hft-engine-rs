use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::Timestamp;

/// 融券券源读数（借券费 + 可借量）
///
/// 由数据源 (如 IBKR shortableShares snapshot) 周期推送，作为事件喂进策略
/// (`Strategy::on_borrow_fee`)，与 BBO/funding 同为事件驱动的输入，不走策略侧同步 IO。
#[derive(Debug, Clone)]
pub struct BorrowFee {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 借券费年化 (小数，正 = 付钱)
    pub fee_annual: f64,
    /// 可借量 (shares)
    pub shortable_shares: f64,
    pub timestamp: Timestamp,
}

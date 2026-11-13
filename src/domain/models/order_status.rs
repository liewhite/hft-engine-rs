use serde::{Deserialize, Serialize};

/// 订单状态（交易所说的）。本地「已发出、交易所还没说过话」不是订单状态，见 `messaging::OrderStage::Placing`
///
/// 本地 REST 调用的结论（下单被拒、撤单失败、下单已受理）不是订单状态，走
/// [`crate::messaging::OrderRequestResult`]。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OrderStatus {
    /// 交易所已收到，等待成交
    Pending,
    /// 部分成交（累计量见 [`crate::domain::OrderUpdate::filled_quantity`]）
    PartiallyFilled,
    /// 完全成交
    Filled,
    /// 已取消
    Cancelled,
    /// 交易所拒单（含币安 EXPIRED 这类过期作废）
    Rejected { reason: String },
}

impl OrderStatus {
    /// 是否为终态 (订单生命周期结束；成交可能还没全部入账，见 `messaging::state::PendingOrder`)
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected { .. }
        )
    }
}

use crate::domain::types::Quantity;
use serde::{Deserialize, Serialize};

/// 订单状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    PartiallyFilled { filled: Quantity },
    Filled,
    Cancelled,
    Rejected { reason: String },
    /// 下单请求发送失败（REST API 错误）
    Error { reason: String },
}

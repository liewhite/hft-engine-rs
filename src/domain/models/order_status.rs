use crate::domain::types::Quantity;
use serde::{Deserialize, Serialize};

/// 订单状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OrderStatus {
    /// 本地已创建，等待交易所响应
    Created,
    /// 交易所已收到，等待成交
    Pending,
    /// 部分成交
    PartiallyFilled { filled: Quantity },
    /// 完全成交
    Filled,
    /// 已取消
    Cancelled,
    /// 被拒绝
    Rejected { reason: String },
    /// 下单请求发送失败（REST API 错误）
    Error { reason: String },
}

impl OrderStatus {
    /// 是否为终态 (订单生命周期结束)
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OrderStatus::Filled
                | OrderStatus::Cancelled
                | OrderStatus::Rejected { .. }
                | OrderStatus::Error { .. }
        )
    }

    /// 是否已被交易所确认 (非 Created 状态)
    pub fn is_confirmed(&self) -> bool {
        !matches!(self, OrderStatus::Created)
    }
}

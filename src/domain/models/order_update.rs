use crate::domain::models::{Exchange, OrderStatus, Side, Symbol};
use crate::domain::types::{OrderId, Quantity, Timestamp};

/// 订单更新事件
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub client_order_id: Option<String>,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub status: OrderStatus,
    /// 累计成交量
    pub filled_quantity: Quantity,
    /// 本次成交量（用于乐观更新 position）
    pub fill_sz: Quantity,
    pub timestamp: Timestamp,
}

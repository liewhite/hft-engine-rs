use crate::domain::models::{Exchange, OrderStatus, Side, Symbol};
use crate::domain::types::{OrderId, Price, Quantity, Timestamp};

/// 订单更新事件
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub client_order_id: Option<String>,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub status: OrderStatus,
    pub filled_quantity: Quantity,
    pub avg_price: Option<Price>,
    pub timestamp: Timestamp,
}

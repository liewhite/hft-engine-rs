use crate::domain::models::{Exchange, OrderType, Side, Symbol};
use crate::domain::types::{OrderId, Quantity};

/// 订单
#[derive(Debug, Clone)]
pub struct Order {
    pub id: OrderId,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}

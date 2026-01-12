use crate::domain::models::{Exchange, Side, Symbol};
use crate::domain::types::{Price, Quantity, Timestamp};

/// 成交事件
#[derive(Debug, Clone)]
pub struct Fill {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    /// 成交价格
    pub price: Price,
    /// 成交数量
    pub size: Quantity,
    /// 客户端订单ID（用于关联 PendingOrder）
    pub client_order_id: Option<String>,
    /// 交易所订单ID
    pub order_id: String,
    pub timestamp: Timestamp,
}

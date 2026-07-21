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
    /// 订单价格 (限价单)
    pub price: f64,
    /// 是否 reduce-only（只减仓）单。
    /// Binance/OKX 的订单推送含此字段并如实填充；HL/IBKR 的 WS 订单消息不提供，填 `false`。
    /// 主要用于重连/启动时把交易所现存挂单准确注册为 PendingOrder（避免把平仓单当开仓单）。
    pub reduce_only: bool,
    /// 订单总数量 (币单位)
    pub quantity: Quantity,
    /// 累计成交量
    pub filled_quantity: Quantity,
    /// 本次成交量（用于乐观更新 position）
    pub fill_sz: Quantity,
    pub timestamp: Timestamp,
}

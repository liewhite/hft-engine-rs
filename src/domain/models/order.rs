use crate::domain::models::{Exchange, OrderType, Side, Symbol};
use crate::domain::types::Quantity;

/// 策略要下的一张单（下单意图）：在哪所、哪个币、什么方向和类型、多少（币本位，未取整）、是否只减仓。
///
/// 没有订单号，也不必按交易所精度取整：框架（`StrategyRunner`）查过它在策略的订阅范围内后，
/// 分配客户端订单号、按交易所精度取整，得到 [`Order`]。
#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub reduce_only: bool,
}

/// 框架发出的一张单：已分配客户端订单号、价格与数量已按交易所精度取整（仍是币本位，
/// 折成交易所单位是出口的事，见 `ExchangeOrder`）。只由 `StrategyRunner` 从 [`OrderIntent`] 造出
/// （测试除外）；交易所订单号要等交易所受理后才有，记在订单跟踪表里，不在这里。
#[derive(Debug, Clone)]
pub struct Order {
    pub client_order_id: String,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub reduce_only: bool,
}

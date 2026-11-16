use crate::domain::models::{Exchange, OrderStatus, Side, Symbol};
use crate::domain::types::{OrderId, Quantity};

/// 交易所的订单回报：**只带跟踪一张单所必需的字段**。
///
/// - `client_order_id`：本地跟踪表的主键、撤单的依据；交易所可能不给（如部分系统单）则为 `None`。
///   账户归本引擎独占（见 `docs/architecture.md`「账户独占」），不按它分辨是谁的单；
/// - `order_id`：日志与人工排查（撤单按 `client_order_id`，见 `AccountClient::cancel_order`）；
/// - `status`：生命周期；
/// - `side` 与 `quantity`：策略比对是否该撤单重挂；
/// - `filled_quantity`：**交易所说的累计成交量**（只用于日志排查）。订单列表不看成交、持仓不看订单回报（#899）：
///   成交与订单回报在有的所走两个频道（HL 的 userFills / orderUpdates），终态回报可能先于成交到达。
///
/// 成交明细（价格、手续费）走 [`crate::domain::Fill`]，不在这里捎带。
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub client_order_id: Option<String>,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub status: OrderStatus,
    /// 订单总数量 (币单位)
    pub quantity: Quantity,
    /// 交易所报的累计成交量 (币单位)：BN `z`、OKX `accFillSz`（张折币）、HL `origSz - sz`、
    /// 模拟柜台自己的累计
    pub filled_quantity: Quantity,
}

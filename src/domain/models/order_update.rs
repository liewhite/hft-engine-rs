use crate::domain::models::{Exchange, OrderStatus, Side, Symbol};
use crate::domain::types::{OrderId, Quantity};

/// 交易所的订单回报：**只带跟踪一张单所必需的字段**。
///
/// - `client_order_id`：本引擎的单才有（本地跟踪表的主键）；别人的单（人工、其他程序、强平等系统单）
///   为 `None` —— 归属由各所适配层解析回报时判定（见 `crate::exchange::CliOrderIdFormat`），
///   框架核心与策略见到 `Some` 就是本引擎的单，不再自己判断；
/// - `order_id`：日志与人工排查（撤单按 `client_order_id`，见 `AccountClient::cancel_order`）；
/// - `status`：生命周期；
/// - `side` 与 `quantity`：策略比对是否该撤单重挂；
/// - `filled_quantity`：**交易所说的累计成交量**。本地跟踪表按客户端订单号把收到的成交（[`crate::domain::Fill`]）
///   加起来，加满这个数才认为这张单「结算完」、从表里移出（见 `messaging::state::PendingOrder`）。
///   成交与订单回报在有的所走两个频道（HL 的 userFills / orderUpdates），终态回报可能先于成交到达；
///   没有这个数，就分不清「单结束了、成交都入账了」和「单结束了、成交还在路上」。
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
    /// IBKR `filledQuantity`、模拟柜台自己的累计
    pub filled_quantity: Quantity,
}

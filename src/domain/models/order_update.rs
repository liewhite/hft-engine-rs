use crate::domain::models::{Exchange, OrderStatus, Side, Symbol};
use crate::domain::types::{OrderId, Quantity};

/// 订单更新事件：**只带跟踪一张挂单所必需的字段**。
///
/// 挂单跟踪实际只用到：`client_order_id`（pending 表主键、启动期归属识别）、`order_id`
/// （撤单与撤单复查）、`status`（生命周期）、`side` 与 `quantity`（策略比对是否该撤单重挂）。
/// 成交明细走 [`crate::domain::Fill`]，账户盈亏走账本 —— 都不需要挂单回报捎带。
///
/// # 曾经在这里、已删掉的五个字段
///
/// 它们零读者，却要四个交易所各解析一遍，而各所给的根本不是一回事：
///
/// - `fill_sz`（本次成交量）：Hyperliquid 没有这个概念，适配层**拿累计量冒充**；IBKR 的
///   sor 推送也没有，三处写死 0。一个"四所里两所在编"的字段。
/// - `reduce_only`：OKX 要做三态字符串解析（`Option<String>` + `== "true"`）、REST 与 WS
///   口径还不一致；HL / IBKR 共四处写死 `false` 外加四条解释性注释。文档曾声称它用于
///   "重连时把现存挂单准确注册"，但没有任何消费方读过重建后的这个值。
/// - `timestamp`：OKX/Binance 填本地墙钟、HL 填交易所时间、IBKR 填接收时刻 —— **两种语义
///   混在一个字段里**，而它唯一的写入落在一条永不可达的超时分支上。
/// - `filled_quantity` 与 `OrderStatus::PartiallyFilled` 的载荷：一起删才有意义，否则
///   各所的累计量解析（OKX 的张→币折算、HL 的 `origSz - sz`、Binance 的 `z`）原样保留。
/// - `price`：唯一"读者"是一条为 IBKR 写死 0 而设的空值守卫，那条守卫判 `quantity` 即可。
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
}

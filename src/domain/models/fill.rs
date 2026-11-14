use crate::domain::models::{Exchange, Money, Side, Symbol};
use crate::domain::types::{Price, Quantity, Timestamp};

/// 成交来源/原因
///
/// 强平 (Liquidation) 与自动减仓 (ADL) 都是被动的强制成交——它们和主动下单一样经交易所
/// 私有成交流下发，因此走同一 `Fill` 路径更新本地持仓（见 `SymbolState::apply` 的 Fill 分支）。
/// 该枚举仅用于**可观测性**：把被动成交与主动成交区分开，便于日志排查，不影响持仓计算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillReason {
    /// 主动成交（策略单 / 手动单）
    #[default]
    Normal,
    /// 强平
    Liquidation,
    /// 自动减仓 (Auto-Deleveraging)
    Adl,
}

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
    /// 客户端订单号（用于关联 PendingOrder）；交易所没给为 `None`
    pub client_order_id: Option<String>,
    /// 交易所订单ID
    pub order_id: String,
    pub timestamp: Timestamp,
    /// 手续费：正数 = 收费，负数 = 返佣；`None` = 这笔没收手续费。
    ///
    /// 币种如实照搬交易所回报，不假定是计价币、也不换算（#817）：币安开了 BNB 抵扣时是 BNB，
    /// 现货买卖两边扣的币也不同。汇总时按币种分开记（见 [`super::TradingStats::fees`]）。
    pub fee: Option<Money>,
    /// 成交来源（主动 / 强平 / ADL）——仅用于可观测性
    pub reason: FillReason,
}

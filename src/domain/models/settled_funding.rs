use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Rate, Timestamp};

/// 一次**已结算**的公共资金费率（交易所公布的历史费率，不是本账户收付的资费）。
///
/// 与 [`super::FundingRate`]（当前周期的预测费率）是两回事：预测费率结算前一直在变，
/// 已结算费率是事实、不会再变。按"过去一段时间实际结算了多少"做决策的策略要的是本类型。
///
/// `rate` 是**该次结算的费率本身**，不折算周期：币安一次 8h 结算 0.01% 就是 0.0001，
/// HL 一次 1h 结算 0.00125% 就是 0.0000125。窗口内逐条相加即得该窗口的累计费率，
/// 结算周期不同的交易所可直接比较（这正是不折算的理由）。
#[derive(Debug, Clone, PartialEq)]
pub struct SettledFunding {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub rate: Rate,
    /// 结算时刻（毫秒）。同一 (exchange, symbol) 下唯一，可作去重键。
    pub settle_time: Timestamp,
}

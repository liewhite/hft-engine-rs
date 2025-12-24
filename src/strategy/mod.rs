mod funding_arb;

pub use funding_arb::{FundingArbConfig, FundingArbStrategy};

use crate::domain::{Exchange, Order, Symbol};
use crate::messaging::{ExchangeEvent, StateManager};

/// 策略需要的市场数据类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarketDataType {
    /// Best Bid/Offer
    BBO,
    /// 资金费率
    FundingRate,
    /// 仓位更新
    Position,
    /// 余额更新
    Balance,
    /// 订单更新
    OrderUpdate,
}

/// 策略输出的信号
#[derive(Debug, Clone)]
pub enum Signal {
    /// 下单信号
    PlaceOrder(Order),
}

/// 策略 trait
///
/// 用户实现此 trait 来定义自己的策略逻辑
pub trait Strategy: Send + Sync {
    /// 策略需要对接的交易所
    fn exchanges(&self) -> Vec<Exchange>;

    /// 策略需要对接的交易对
    fn symbols(&self) -> Vec<Symbol>;

    /// 策略需要的市场数据类型
    fn market_data_types(&self) -> Vec<MarketDataType>;

    /// 订单超时时间 (毫秒)
    fn order_timeout_ms(&self) -> u64;

    /// 处理事件
    ///
    /// state: 状态管理器，提供状态查询和下单接口
    fn on_event(&mut self, event: &ExchangeEvent, state: &mut StateManager);
}

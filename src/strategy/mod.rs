mod funding_arb;
mod macd_grid;
pub(crate) mod metrics_pusher;
mod slack_notifier;
mod spread_arb;

pub use funding_arb::{FundingArbConfig, FundingArbMetricsActor, FundingArbMetricsArgs, FundingArbStrategy};
pub use macd_grid::{MacdGridConfig, MacdGridStrategy};
pub use slack_notifier::{SlackNotifierActor, SlackNotifierArgs};
pub use spread_arb::{
    SpreadArbConfig, SpreadArbMetricsActor, SpreadArbMetricsArgs, SpreadArbStatsActor,
    SpreadArbStatsArgs, SpreadArbStrategy, SpreadPairConfig,
};

use crate::domain::{Exchange, Order, OrderId, Symbol};
use crate::exchange::SubscriptionKind;
use crate::messaging::{IncomeEvent, StateManager};
use std::collections::{HashMap, HashSet};

/// 策略输出的信号
#[derive(Debug, Clone)]
pub enum OutcomeEvent {
    /// 下单信号（一次决策可包含多个关联订单）
    PlaceOrders {
        /// 关联订单列表
        orders: Vec<Order>,
        /// 信号意图描述，如 "spread_open | spread=0.30% | qty=10"
        comment: String,
    },
    /// 撤单信号
    CancelOrder {
        exchange: Exchange,
        symbol: Symbol,
        order_id: OrderId,
    },
}

/// 策略 trait
///
/// 用户实现此 trait 来定义自己的策略逻辑
/// 策略是纯函数式的：接收事件和状态，返回要执行的动作
pub trait Strategy: Send + Sync {
    /// 策略需要订阅的公共数据流
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>>;

    /// 订单超时时间 (毫秒)
    fn order_timeout_ms(&self) -> u64;

    /// 处理事件，可产出零到多个信号
    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent>;
}

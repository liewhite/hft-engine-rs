mod funding_arb;
mod metrics_subscriber;
mod slack_notifier;

pub use funding_arb::{FundingArbConfig, FundingArbStrategy};
pub use metrics_subscriber::{MetricsSubscriberActor, MetricsSubscriberArgs};
pub use slack_notifier::{SlackNotifierActor, SlackNotifierArgs};

use crate::domain::{Exchange, Order};
use crate::exchange::SubscriptionKind;
use crate::messaging::{IncomeEvent, StateManager};
use std::collections::{HashMap, HashSet};

/// 策略输出的信号
#[derive(Debug, Clone)]
pub enum OutcomeEvent {
    /// 下单信号
    PlaceOrder {
        /// 订单
        order: Order,
        /// 订单描述（信号原因、上下文等）
        comment: String,
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

    /// 处理事件，返回要执行的动作
    ///
    /// state: 状态管理器，只读
    /// 返回: 策略产生的信号列表 (如下单)
    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent>;
}

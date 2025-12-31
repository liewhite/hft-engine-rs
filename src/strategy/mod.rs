mod funding_arb;

pub use funding_arb::{FundingArbConfig, FundingArbStrategy};

use crate::domain::{Exchange, Order};
use crate::exchange::SubscriptionKind;
use crate::messaging::{IncomeEvent, StateManager};
use std::collections::{HashMap, HashSet};

/// 公共数据流订阅配置

/// 策略输出的信号
#[derive(Debug, Clone)]
pub enum OutcomeEvent {
    /// 下单信号
    PlaceOrder(Order),
}

/// 策略 trait
///
/// 用户实现此 trait 来定义自己的策略逻辑
pub trait Strategy: Send + Sync {
    /// 策略需要订阅的公共数据流
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>>;

    /// 订单超时时间 (毫秒)
    fn order_timeout_ms(&self) -> u64;

    /// 处理事件
    ///
    /// state: 状态管理器，提供状态查询和下单接口
    fn on_event(&mut self, event: &IncomeEvent, state: &mut StateManager);
}

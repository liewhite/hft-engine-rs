mod gamma_scalp;
mod spread_arb;

pub use spread_arb::{SpreadArbConfig, SpreadArbStrategy, MIN_EXCHANGES_PER_SYMBOL};
pub use gamma_scalp::GammaScalpStrategy;

use crate::domain::{BorrowFee, Exchange, ExchangeRate, Order, OrderId, Symbol};
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

    /// 处理事件，可产出零到多个信号 (BBO/funding/fill/order/clock 等经此)
    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent>;

    /// 券源读数 (借券费 + 可借量) 到达。由外部数据源经注入入口推入的事件驱动，
    /// 与 on_event 一样是"框架喂、策略反应"的纯逻辑；默认忽略，策略按需实现。
    fn on_borrow_fee(&mut self, _fee: &BorrowFee, _state: &StateManager) -> Vec<OutcomeEvent> {
        Vec::new()
    }

    /// 汇率读数到达。默认忽略，策略按需实现。
    fn on_exchange_rate(&mut self, _rate: &ExchangeRate, _state: &StateManager) -> Vec<OutcomeEvent> {
        Vec::new()
    }
}

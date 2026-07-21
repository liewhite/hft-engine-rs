//! 撮合核心 (确定性纯状态机)：账本 + 挂单簿 + 撮合判定。
//!
//! 全部撮合逻辑都是 [`SimState`] 上的确定性转移，脱离线程/锁/延迟即可同步单测。
//! 回测引擎 ([`crate::backtest`]) 在单线程虚拟时间循环里串行驱动这些转移。

mod config;
mod ledger;
mod matcher;
mod state;

pub use config::SimConfig;
pub use ledger::Ledger;
pub use matcher::{crosses, touch_price, trade_crosses};
pub use state::{Liquidity, RestingOrder, SimState};

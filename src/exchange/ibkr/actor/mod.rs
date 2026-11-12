//! IBKR 节点模块
//!
//! 树状结构:
//! IbkrNode
//! ├── IbkrPublicWs（行情 smd + 订单 sor + 成交 str，同一条 WS）
//! ├── IbkrTickle
//! ├── EquityPoller（通用，见 crate::exchange::equity_polling）
//! ├── IbkrStatusPoller
//! └── IbkrSnapshotPoller（可选）

pub(crate) mod ibkr_actor;
mod public_ws;
pub(crate) mod snapshot_polling;
mod status_polling;
mod tickle;

pub use ibkr_actor::{IbkrArgs, IbkrNode};
pub use snapshot_polling::IbkrSnapshotConfig;

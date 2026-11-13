//! OKX 节点模块
//!
//! 树状结构:
//! OkxNode
//! ├── OkxPublicWs（第一条订阅时才连）
//! ├── OkxBusinessWs（K线数据，第一条 K 线订阅时才连）
//! ├── OkxPrivateWs（需要凭证）
//! ├── EquityPoller（通用，见 crate::exchange::equity_polling）
//! ├── FundingFeePoller（通用，见 crate::exchange::funding_fee_polling）
//! └── CryptoStatus
//!
//! OkxGreeksPoller（REST 轮询希腊值）不在节点下：策略声明要希腊值时才由投产编排起
//! （见 `crate::exchange::AccountStream`）。

mod business_ws;
mod greeks_polling;
mod okx_actor;
mod private_ws;
pub(crate) mod public_ws;

pub use okx_actor::{OkxArgs, OkxNode};
pub(crate) use greeks_polling::{OkxGreeksPoller, GREEKS_POLLING_INTERVAL_MS};

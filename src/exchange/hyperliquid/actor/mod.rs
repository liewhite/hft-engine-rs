//! Hyperliquid 节点模块
//!
//! 树状结构:
//! HyperliquidNode
//! ├── HyperliquidPublicWs
//! ├── HyperliquidPrivateWs（需要凭证）
//! ├── EquityPoller（通用，见 crate::exchange::equity_polling）
//! ├── HyperliquidFundingFeePoller
//! └── CryptoStatus

mod funding_fee_polling;
mod hyperliquid_actor;
pub(super) mod private_ws;
pub(crate) mod public_ws;

pub use hyperliquid_actor::{HyperliquidArgs, HyperliquidNode};

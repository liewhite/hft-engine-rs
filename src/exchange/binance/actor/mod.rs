//! Binance 节点模块
//!
//! 树状结构:
//! BinanceNode
//! ├── BinancePublicWs ×2（/public/ws、/market/ws）
//! ├── BinancePrivateWs（需要凭证）
//! │   └── ListenKeyRefresher
//! ├── EquityPoller（通用，见 crate::exchange::equity_polling）
//! ├── FundingFeePoller（通用，见 crate::exchange::funding_fee_polling）
//! └── CryptoStatus

pub(crate) mod binance_actor;
mod listen_key;
mod private_ws;
pub(crate) mod public_ws;

pub use binance_actor::{BinanceArgs, BinanceNode};

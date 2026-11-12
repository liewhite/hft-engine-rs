//! Binance 节点模块
//!
//! 树状结构:
//! BinanceNode
//! ├── BinancePublicWs ×2（/public/ws、/market/ws）
//! ├── BinancePrivateWs（需要凭证）
//! │   └── ListenKeyRefresher
//! ├── EquityPoller（通用，见 crate::exchange::equity_polling）
//! ├── BinanceFundingFeePoller
//! └── CryptoStatus

pub(crate) mod binance_actor;
mod funding_fee_polling;
mod listen_key;
mod private_ws;
pub(crate) mod public_ws;

pub use binance_actor::{BinanceArgs, BinanceNode};

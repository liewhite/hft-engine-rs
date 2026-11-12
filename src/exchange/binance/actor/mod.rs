//! Binance Actor 模块
//!
//! 树状结构:
//! BinanceActor (父)
//! ├── BinancePublicWsActor [spawn_link]
//! ├── BinancePrivateWsActor [spawn_link]
//! │   └── BinanceListenKeyActor [spawn_link]
//! ├── EquityPollingActor [spawn_link]（通用，见 crate::exchange::equity_polling）
//! └── BinanceFundingFeePollingActor [spawn_link]

pub(crate) mod binance_actor;
mod funding_fee_polling;
mod listen_key;
mod private_ws;
pub(crate) mod public_ws;

pub use binance_actor::{BinanceActor, BinanceActorArgs};

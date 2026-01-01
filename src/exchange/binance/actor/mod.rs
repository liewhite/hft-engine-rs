//! Binance Actor 模块
//!
//! 树状结构:
//! BinanceActor (父)
//! ├── BinancePublicWsActor [spawn_link]
//! └── BinancePrivateWsActor [spawn_link]
//!     └── BinanceListenKeyActor [spawn_link]

mod binance_actor;
mod listen_key;
mod private_ws;
mod public_ws;

pub use binance_actor::{BinanceActor, BinanceActorArgs, WsData};

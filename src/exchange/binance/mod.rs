pub(crate) mod actor;
pub(crate) mod codec;
mod client;
mod module;
mod symbol;

pub use actor::{BinanceActor, BinanceActorArgs};
pub use client::BinanceClient;
pub use module::BinanceModule;
pub use symbol::{from_binance, to_binance};

/// Binance 永续合约 REST API 地址
pub const REST_BASE_URL: &str = "https://fapi.binance.com";

/// Binance 永续合约 WebSocket 地址
pub const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";

/// Binance 凭证
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BinanceCredentials {
    pub api_key: String,
    pub secret: String,
}

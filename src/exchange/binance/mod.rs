pub(crate) mod codec;
mod config;
mod rest;

pub use config::{BinanceConfig, BinanceCredentials};
pub use rest::BinanceRestClient;

/// Binance 永续合约 REST API 地址
pub const REST_BASE_URL: &str = "https://fapi.binance.com";

/// Binance 永续合约 WebSocket 地址
pub const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";

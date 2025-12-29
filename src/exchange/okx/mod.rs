pub(crate) mod codec;
mod ws;
mod rest;

pub use ws::{OkxWsProtocol, OkxCredentials};
pub use rest::OkxRestClient;

/// OKX REST API 地址
pub const REST_BASE_URL: &str = "https://www.okx.com";

/// OKX WebSocket 地址
pub const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
pub const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";

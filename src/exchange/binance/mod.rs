pub(crate) mod actor;
pub(crate) mod codec;
mod client;
mod symbol;

pub use actor::{BinanceActor, BinanceActorArgs};
pub use client::BinanceClient;
pub use symbol::{from_binance, to_binance};

/// Binance 永续合约 REST API 地址
pub const REST_BASE_URL: &str = "https://fapi.binance.com";

/// Binance USDS-M Futures Public WS（高频公共数据：bookTicker、depth、aggTrade 等）
pub const WS_PUBLIC_HIGH_FREQ_URL: &str = "wss://fstream.binance.com/public/ws";

/// Binance USDS-M Futures Market WS（常规市场数据：markPrice、kline、ticker 等）
pub const WS_MARKET_URL: &str = "wss://fstream.binance.com/market/ws";

/// Binance USDS-M Futures Private WS（用户数据流，需拼 ?listenKey={key}）
pub const WS_PRIVATE_URL: &str = "wss://fstream.binance.com/private/ws";

/// Binance 凭证
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BinanceCredentials {
    pub api_key: String,
    pub secret: String,
    /// 计价币种 (e.g., "USDT", "USDC")
    pub quote: String,
}

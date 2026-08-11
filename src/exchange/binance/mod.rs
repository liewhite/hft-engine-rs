pub(crate) mod actor;
pub(crate) mod codec;
mod client;
mod symbol;

pub use actor::{BinanceActor, BinanceActorArgs};
pub use client::BinanceClient;
pub use symbol::{from_binance, to_binance};

/// Binance 永续合约 REST API 地址
pub const REST_BASE_URL: &str = "https://fapi.binance.com";

/// Binance USDS-M Futures Public WS（盘口类高频数据：bookTicker、depth）
///
/// 注意：`aggTrade` **不在**本端点，在 [`WS_MARKET_URL`]。迁移后 Binance 按路由路径分流，
/// 订错端点的表现是"订阅被 ack (`{"result":null}`) 但永不推数据"——静默无数据，不报错。
/// 实测见 `exchange::trades_conformance::binance_agg_trade_matches_codec`。
pub const WS_PUBLIC_HIGH_FREQ_URL: &str = "wss://fstream.binance.com/public/ws";

/// Binance USDS-M Futures Market WS（markPrice、kline、ticker、**aggTrade**）
pub const WS_MARKET_URL: &str = "wss://fstream.binance.com/market/ws";

/// Binance USDS-M Futures Private WS（用户数据流，需拼 ?listenKey={key}）
pub const WS_PRIVATE_URL: &str = "wss://fstream.binance.com/private/ws";

/// Binance 凭证
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BinanceCredentials {
    pub api_key: String,
    pub secret: String,
}

/// 本所适配层是否实现了这种公共订阅（**能力查询**，装配期由 `ExchangeSetup` 携带）。
///
/// **单一出处**：直接问 public WS 的 kind 映射函数（订阅报文由它生成，返回 None 即
/// 不支持），不另存一份需要手工同步的能力副本。支持与否只取决于 kind 变体，
/// 与 quote 无关 —— 传占位值即可。
pub fn supports_subscription(kind: &crate::exchange::SubscriptionKind) -> bool {
    actor::public_ws::kind_to_stream(kind, "USDT").is_some()
}

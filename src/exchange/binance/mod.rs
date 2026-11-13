pub(crate) mod actor;
pub(crate) mod codec;
mod client;
mod symbol;

pub use actor::{BinanceArgs, BinanceNode};
pub use client::BinanceClient;
pub use symbol::{from_binance, to_binance};

/// 本引擎在 Binance 的订单号：`0x` + 32 hex
pub const CLIENT_ORDER_IDS: super::CliOrderIdFormat = super::CliOrderIdFormat::new("0x", 32);

/// Binance 永续合约 REST API 地址
pub const REST_BASE_URL: &str = "https://fapi.binance.com";

// ---------------------------------------------------------------------------
// 限频（#791，机制见 crate::exchange::rate_limit）。总额度取自 `fapi/v1/exchangeInfo` 的
// rateLimits（2026-11 实查），各接口花多少写在调用处、取自官方文档。连续超 IP 权重会被 418 封 IP。
// ---------------------------------------------------------------------------

use crate::exchange::rate_limit::{Quota, RateLimiter};
use std::time::Duration;

/// 本进程访问币安的唯一限频闸门（IP 与账户额度在进程内共用）
pub(crate) static LIMITER: std::sync::LazyLock<RateLimiter> =
    std::sync::LazyLock::new(|| RateLimiter::new(crate::domain::Exchange::Binance));
/// 按 IP 的请求权重：2400 / 分钟（REQUEST_WEIGHT）
pub(crate) static IP_WEIGHT: Quota = Quota::new("IP 权重/分钟", 2400, Duration::from_secs(60));
/// 按账户的下单数：300 / 10 秒（ORDERS）
pub(crate) static ORDERS_10S: Quota = Quota::new("下单数/10秒", 300, Duration::from_secs(10));
/// 按账户的下单数：1200 / 分钟（ORDERS）
pub(crate) static ORDERS_1M: Quota = Quota::new("下单数/分钟", 1200, Duration::from_secs(60));
/// `fapi/v1/fundingRate` 与 fundingInfo 共用：500 次 / 5 分钟 / IP（另计；是否同时占 IP 权重文档没写，按占 1 算）
pub(crate) static FUNDING_RATE: Quota = Quota::new("fundingRate 次数/5分钟", 500, Duration::from_secs(300));

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

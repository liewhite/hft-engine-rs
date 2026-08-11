pub(crate) mod actor;
pub(crate) mod codec;
mod client;
pub(crate) mod signing;
mod symbol;

pub use actor::{HyperliquidActor, HyperliquidActorArgs};
pub use client::HyperliquidClient;
pub use symbol::{from_hyperliquid, to_hyperliquid};

/// Hyperliquid REST API 地址
pub const REST_BASE_URL: &str = "https://api.hyperliquid.xyz";

/// Hyperliquid WebSocket 地址
pub const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";

/// Hyperliquid 凭证
/// 使用私钥进行 EIP-712 签名
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HyperliquidCredentials {
    /// 钱包地址 (0x...)
    pub wallet_address: String,
    /// 私钥 (不含 0x 前缀)
    pub private_key: String,
}

impl HyperliquidCredentials {
}

/// 本所适配层是否实现了这种公共订阅（**能力查询**，装配期由 `ExchangeSetup` 携带）。
/// 派生自 public WS 的 kind 映射函数，见各所同名函数的约定。
pub fn supports_subscription(kind: &crate::exchange::SubscriptionKind) -> bool {
    actor::public_ws::kind_to_stream(kind, "USDC", "").is_some()
}

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
    /// 计价币种 (e.g., "USDC", "USDE")
    pub quote: String,
    /// Perp DEX 名称 (e.g., "xyz" 表示股票永续合约, "" 表示默认 perp DEX)
    #[serde(default)]
    pub dex: String,
}

impl HyperliquidCredentials {
    /// 将 base symbol (e.g., "AAPL") 转换为 HL 侧 symbol (e.g., "xyz:AAPL")
    ///
    /// 当 dex 为空时返回原始 symbol，否则拼接 "{dex}:{symbol}"。
    pub fn hl_symbol(&self, symbol: &str) -> String {
        if self.dex.is_empty() {
            symbol.to_string()
        } else {
            format!("{}:{}", self.dex, symbol)
        }
    }
}

pub(crate) mod actor;
mod client;
pub mod oauth;
mod symbol;

pub use actor::{IbkrActor, IbkrActorArgs};
pub use client::IbkrClient;

/// IBKR REST API 基础 URL
pub const REST_BASE_URL: &str = "https://api.ibkr.com/v1/api/";

/// IBKR 凭证
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IbkrCredentials {
    pub consumer_key: String,
    pub encryption_key_fp: String,
    pub signature_key_fp: String,
    pub access_token: String,
    pub access_token_secret: String,
    pub dh_prime: String,
    /// 要交易的 symbol 列表 (e.g., ["AAPL", "NVDA"])
    pub symbols: Vec<String>,
}

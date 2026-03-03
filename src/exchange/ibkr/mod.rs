pub(crate) mod actor;
pub mod auth;
pub(crate) mod client;
mod gateway;
pub mod oauth;
mod symbol;

pub use actor::{IbkrActor, IbkrActorArgs};
pub use auth::IbkrAuth;
pub use client::IbkrClient;

use gateway::IbkrGateway;
use oauth::IbkrOAuth;
use std::sync::Arc;

/// IBKR OAuth REST API 基础 URL
pub const OAUTH_BASE_URL: &str = "https://api.ibkr.com/v1/api/";

fn default_gateway_url() -> String {
    "https://localhost:5000/v1/api/".to_string()
}

/// IBKR 凭证 (tagged enum: OAuth 或 Gateway)
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "mode")]
pub enum IbkrCredentials {
    #[serde(rename = "oauth")]
    OAuth {
        consumer_key: String,
        encryption_key_fp: String,
        signature_key_fp: String,
        access_token: String,
        access_token_secret: String,
        dh_prime: String,
        symbols: Vec<String>,
    },
    #[serde(rename = "gateway")]
    Gateway {
        #[serde(default = "default_gateway_url")]
        base_url: String,
        symbols: Vec<String>,
    },
}

impl IbkrCredentials {
    /// 获取 symbol 列表
    pub fn symbols(&self) -> &[String] {
        match self {
            Self::OAuth { symbols, .. } => symbols,
            Self::Gateway { symbols, .. } => symbols,
        }
    }

    /// 创建对应模式的认证器
    pub async fn create_auth(&self) -> anyhow::Result<Arc<dyn IbkrAuth>> {
        match self {
            Self::OAuth {
                consumer_key,
                encryption_key_fp,
                signature_key_fp,
                access_token,
                access_token_secret,
                dh_prime,
                ..
            } => {
                let oauth = IbkrOAuth::create(
                    consumer_key,
                    encryption_key_fp,
                    signature_key_fp,
                    access_token,
                    access_token_secret,
                    dh_prime,
                )
                .await?;
                Ok(Arc::new(oauth) as Arc<dyn IbkrAuth>)
            }
            Self::Gateway { base_url, .. } => {
                Ok(Arc::new(IbkrGateway::new(base_url.clone())) as Arc<dyn IbkrAuth>)
            }
        }
    }
}

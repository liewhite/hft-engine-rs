pub(crate) mod actor;
pub(crate) mod codec;
mod client;
mod symbol;

pub use actor::{OkxActor, OkxActorArgs};
pub use client::OkxClient;
pub use symbol::{from_okx, to_okx};

use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// OKX REST API 地址
pub const REST_BASE_URL: &str = "https://www.okx.com";

/// OKX WebSocket 地址
pub const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
pub const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";

/// OKX 凭证
#[derive(Clone)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl OkxCredentials {
    /// WebSocket 登录签名
    pub fn sign_ws_login(&self, timestamp: &str) -> String {
        let message = format!("{}GET/users/self/verify", timestamp);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any size");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        general_purpose::STANDARD.encode(result.into_bytes())
    }
}

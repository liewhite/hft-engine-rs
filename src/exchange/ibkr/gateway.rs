//! IbkrGateway — 通过本地 IB Gateway/TWS 连接 IBKR
//!
//! 无 OAuth 签名，本地自签名证书需跳过验证。

use super::auth::IbkrAuth;
use std::time::Duration;

pub struct IbkrGateway {
    base_url: String,
}

impl IbkrGateway {
    pub fn new(base_url: String) -> Self {
        Self { base_url }
    }
}

impl IbkrAuth for IbkrGateway {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn ws_url(&self) -> String {
        let ws_base = self.base_url.replace("https://", "wss://").replace("http://", "ws://");
        format!("{}ws", ws_base)
    }

    fn sign_request(
        &self,
        _method: &str,
        _url: &str,
        _params: Option<&[(&str, &str)]>,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    fn build_http_client(&self) -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(true)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))
    }

    fn ws_connector(&self) -> Option<tokio_tungstenite::Connector> {
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("Failed to build TLS connector");
        Some(tokio_tungstenite::Connector::NativeTls(tls))
    }

    fn format_ws_cookie(&self, session_id: &str) -> String {
        format!("api={}", serde_json::json!({"session": session_id}))
    }
}

//! ListenKeyRefresher - 定时续期 Binance ListenKey
//!
//! 续期失败即出错退出，整支（私有 WS 及以上）停下

use crate::exchange::client::WsError;
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::time::Duration;

/// ListenKey 刷新间隔 (30 分钟)
const LISTEN_KEY_REFRESH_INTERVAL_SECS: u64 = 30 * 60;

/// 定时续期 ListenKey
pub(super) struct ListenKeyRefresher {
    /// REST API 基础 URL
    rest_base_url: String,
    /// API Key
    api_key: String,
    /// HTTP 客户端
    client: reqwest::Client,
}

impl ListenKeyRefresher {
    /// `client` 由私有 WS 传入（与创建 ListenKey 同一个，带请求超时）
    pub(super) fn spawn(
        group: &Group,
        client: reqwest::Client,
        rest_base_url: String,
        api_key: String,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!("Binance ListenKeyRefresher started");
        group.spawn_periodic(
            "BinanceListenKeyRefresher",
            Duration::from_secs(LISTEN_KEY_REFRESH_INTERVAL_SECS),
            Self { rest_base_url, api_key, client },
        )
    }

    /// 刷新 ListenKey
    async fn refresh_listen_key(&self) -> Result<(), WsError> {
        let resp = self
            .client
            .put(format!("{}/fapi/v1/listenKey", self.rest_base_url))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| WsError::AuthFailed(e.to_string()))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(WsError::AuthFailed(format!(
                "ListenKey refresh failed: {}",
                text
            )));
        }

        tracing::debug!("Binance ListenKey refreshed");
        Ok(())
    }
}

impl Periodic for ListenKeyRefresher {
    async fn tick(&mut self) -> anyhow::Result<()> {
        Ok(self.refresh_listen_key().await?)
    }
}

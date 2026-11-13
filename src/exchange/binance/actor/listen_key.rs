//! ListenKeyRefresher - 定时续期 Binance ListenKey
//!
//! ListenKey 每次续期后有效 60 分钟。续期每 5 分钟一次：偶发失败（网络抖动）记 warn、下拍再试；
//! 距上次成功超过 50 分钟仍续不上才出错退出，整支（私有 WS 及以上）停下 —— 再拖下去 ListenKey
//! 过期，私有流会静默断（#767：此前 30 分钟一次、失败一次就停机）。

use crate::domain::ExchangeError;
use crate::exchange::staleness::StalenessGuard;
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::time::Duration;

/// 续期间隔
const LISTEN_KEY_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 允许连续续不上的最长时间：ListenKey 有效期 60 分钟，留 10 分钟余量
const LISTEN_KEY_MAX_FAILING_MS: u64 = 50 * 60 * 1000;

/// 定时续期 ListenKey
pub(super) struct ListenKeyRefresher {
    /// REST API 基础 URL
    rest_base_url: String,
    /// API Key
    api_key: String,
    /// HTTP 客户端
    client: reqwest::Client,
    /// 距上次续期成功的时长守卫（ListenKey 刚创建时视作刚成功过）
    guard: StalenessGuard,
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
            LISTEN_KEY_REFRESH_INTERVAL,
            Self {
                rest_base_url,
                api_key,
                client,
                guard: StalenessGuard::new("Binance ListenKey 续期", LISTEN_KEY_MAX_FAILING_MS),
            },
        )
    }

    /// 刷新 ListenKey
    async fn refresh_listen_key(&self) -> Result<(), ExchangeError> {
        let admitted = crate::exchange::binance::LIMITER
            .query(&[crate::exchange::binance::IP_WEIGHT.cost(1)])
            .await
            .map_err(|e| ExchangeError::new(format!("鉴权失败: {}", e)))?;
        let resp = crate::net::send(
            self.client
                .put(format!("{}/fapi/v1/listenKey", self.rest_base_url))
                .header("X-MBX-APIKEY", &self.api_key),
            admitted,
        )
        .await
        .map_err(|e| ExchangeError::new(format!("鉴权失败: {}", e)))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ExchangeError::new(format!(
                "ListenKey refresh failed: {}",
                text
            )));
        }

        tracing::debug!("Binance ListenKey refreshed");
        Ok(())
    }
}

impl Periodic for ListenKeyRefresher {
    /// `Err` = 已连续续不上 50 分钟，ListenKey 即将过期，整支停
    async fn tick(&mut self) -> anyhow::Result<()> {
        match self.refresh_listen_key().await {
            Ok(()) => self.guard.record_success(),
            Err(e) => {
                self.guard
                    .check_failure(&e)
                    .map_err(|reason| anyhow::anyhow!("{reason}（私有流即将失效）"))?;
                tracing::warn!(error = %e, failing_ms = self.guard.stale_ms(), "Binance ListenKey 续期失败，下拍重试");
            }
        }
        Ok(())
    }
}

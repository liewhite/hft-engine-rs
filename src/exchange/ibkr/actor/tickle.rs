//! IbkrTickle - IBKR tickle 保活
//!
//! 定时发送 POST /tickle 维持 session。连续失败 3 次即出错退出，整支（IbkrNode 及以上）停下。

use crate::exchange::ibkr::auth::{self, IbkrAuth};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::sync::Arc;
use std::time::Duration;

/// 连续失败阈值
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Tickle 间隔 (秒)
const TICKLE_INTERVAL_SECS: u64 = 60;

/// tickle 保活
pub(super) struct IbkrTickle {
    auth: Arc<dyn IbkrAuth>,
    http: reqwest::Client,
    consecutive_failures: u32,
}

impl IbkrTickle {
    /// 在 `group` 下起保活，每 60 秒一次（首次立即）
    pub(super) fn spawn(
        group: &Group,
        auth: Arc<dyn IbkrAuth>,
        http: reqwest::Client,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!("IbkrTickle started");
        group.spawn_periodic(
            "IbkrTickle",
            Duration::from_secs(TICKLE_INTERVAL_SECS),
            Self { auth, http, consecutive_failures: 0 },
        )
    }
}

impl Periodic for IbkrTickle {
    async fn tick(&mut self) -> anyhow::Result<()> {
        match auth::tickle(&*self.auth, &self.http).await {
            Ok(_) => {
                self.consecutive_failures = 0;
                tracing::trace!("IBKR tickle sent");
            }
            Err(e) => {
                self.consecutive_failures += 1;
                tracing::warn!(
                    error = %e,
                    consecutive_failures = self.consecutive_failures,
                    max = MAX_CONSECUTIVE_FAILURES,
                    "IBKR tickle failed"
                );
                anyhow::ensure!(
                    self.consecutive_failures < MAX_CONSECUTIVE_FAILURES,
                    "IBKR tickle 连续失败 {MAX_CONSECUTIVE_FAILURES} 次，session 保活失效: {e:#}"
                );
            }
        }
        Ok(())
    }
}

//! OkxGreeksPoller - 定时通过 REST 查询 OKX 账户希腊值
//!
//! OKX account-greeks WebSocket 推送频率太低，改用 REST 轮询。
//! 官方限速 10/2s，配置为每秒 3 次。

use crate::domain::Exchange;
use crate::exchange::okx::OkxClient;
use crate::exchange::staleness::{stamped_on_receipt, StalenessGuard, MAX_POLL_STALENESS_MS};
use crate::messaging::{AccountBus, AccountData, AccountEvent, ExchangeAccountData};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// 定时查询希腊值
pub(super) struct OkxGreeksPoller {
    client: Arc<OkxClient>,
    account_bus: AccountBus,
    /// 缓存每个 ccy 的上次 timestamp，用于去重
    last_ts: HashMap<String, u64>,
    /// 停摆守卫：希腊值取不到时 `StateManager` 里的旧值原样留着，策略拿它算 delta
    /// 敞口却无从分辨新鲜度。长期取不到即退出（见 StalenessGuard）。
    guard: StalenessGuard,
}

impl OkxGreeksPoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）
    pub(super) fn spawn(
        group: &Group,
        client: Arc<OkxClient>,
        account_bus: AccountBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!(exchange = "OKX", interval_ms, "OkxGreeksPoller started");
        group.spawn_periodic(
            "OkxGreeksPoller",
            Duration::from_millis(interval_ms),
            Self {
                client,
                account_bus,
                last_ts: HashMap::new(),
                guard: StalenessGuard::new("OKX 账户希腊值", MAX_POLL_STALENESS_MS),
            },
        )
    }
}

impl Periodic for OkxGreeksPoller {
    /// 拉一次希腊值并发布。`Err` = 已停摆过久，整支停（陈旧读数会让 delta 敞口判断失准）。
    async fn tick(&mut self) -> anyhow::Result<()> {
        // 戳在响应到手之后取，理由见 `stamped_on_receipt`
        let (read, local_ts) = stamped_on_receipt(self.client.fetch_greeks()).await;

        match read {
            Ok(greeks_list) => {
                self.guard.record_success();
                for greeks in greeks_list {
                    // 跳过未变化的数据 (OKX 数据变化时 ts 会更新)
                    let prev_ts = self.last_ts.get(&greeks.ccy).copied().unwrap_or(0);
                    if greeks.timestamp == prev_ts {
                        continue;
                    }
                    self.last_ts.insert(greeks.ccy.clone(), greeks.timestamp);

                    let exchange_ts = greeks.timestamp;
                    self.account_bus.publish(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts,
                        local_ts,
                        data: AccountData::Exchange(ExchangeAccountData::Greeks(greeks)),
                    });
                }
            }
            Err(e) => {
                self.guard.check_failure(&e).map_err(|reason| {
                    anyhow::anyhow!("OKX 希腊值长期取不到（陈旧读数会让 delta 敞口判断失准）: {reason}")
                })?;
                tracing::warn!(
                    exchange = %Exchange::OKX,
                    error = %e,
                    stale_ms = self.guard.stale_ms(),
                    "Failed to fetch greeks, will retry"
                );
            }
        }
        Ok(())
    }
}

//! EquityPoller - 定时用 REST 查账户净值（`AccountClient::fetch_equity`）
//!
//! 用于 WebSocket 不推送净值、或推送的净值口径不对的所：Binance 的 WS 不推 equity；
//! Hyperliquid 统一账户的净值在 spot clearinghouse，按 dex 推送的 clearinghouseState
//! 里恒为 0。所别只体现在 `client.exchange()` 与各 client 的 `fetch_equity` 实现里。

use crate::exchange::staleness::{stamped_on_receipt, StalenessGuard, MAX_POLL_STALENESS_MS};
use crate::exchange::AccountClient;
use crate::messaging::{AccountBus, AccountData, AccountEvent, ExchangeAccountData};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::sync::Arc;
use std::time::Duration;

/// 定时查询 equity
pub struct EquityPoller {
    /// 账户 client
    client: Arc<dyn AccountClient>,
    account_bus: AccountBus,
    /// 停摆守卫：净值取不到时，`StateManager` 里的旧值会原样留着且无从分辨新鲜度 ——
    /// 下游会拿着它一直用下去。长期取不到就让本组件出错退出（见 StalenessGuard）。
    guard: StalenessGuard,
}

impl EquityPoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）。所别取自 `client.exchange()`。
    pub fn spawn(
        group: &Group,
        client: Arc<dyn AccountClient>,
        account_bus: AccountBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        let exchange = client.exchange();
        tracing::info!(%exchange, interval_ms, "EquityPoller started");
        let guard = StalenessGuard::new(format!("{exchange} 账户净值"), MAX_POLL_STALENESS_MS);
        group.spawn_periodic(
            format!("{exchange}EquityPoller"),
            Duration::from_millis(interval_ms),
            Self {
                client,
                account_bus,
                guard,
            },
        )
    }
}

impl Periodic for EquityPoller {
    /// 查一次账户净值。`Err` = 已停摆过久，整支停（陈旧净值会让下游误判）。
    async fn tick(&mut self) -> anyhow::Result<()> {
        // 戳在响应到手之后取，理由见 `stamped_on_receipt`
        let (read, local_ts) = stamped_on_receipt(self.client.fetch_equity()).await;

        match read {
            Ok(equity) => {
                self.guard.record_success();
                self.account_bus.publish(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts: local_ts,
                    local_ts,
                    data: AccountData::Exchange(ExchangeAccountData::Equity {
                        exchange: self.client.exchange(),
                        equity,
                    }),
                });
            }
            Err(e) => {
                self.guard.check_failure(&e).map_err(|reason| {
                    anyhow::anyhow!(
                        "{} 账户净值长期取不到（陈旧净值会让下游误判）: {reason}",
                        self.client.exchange()
                    )
                })?;
                tracing::warn!(
                    exchange = %self.client.exchange(),
                    error = %e,
                    stale_ms = self.guard.stale_ms(),
                    "Failed to fetch equity, will retry"
                );
            }
        }
        Ok(())
    }
}

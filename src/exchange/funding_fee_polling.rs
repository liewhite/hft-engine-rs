//! FundingFeePoller - 定时用 REST 拉本账户的资金费结算明细（`FundingFeeSource::fetch_funding_fees`）
//!
//! 用于 WS 不推、或推送里定位不到币的所：Binance 全仓模式下 ACCOUNT_UPDATE 的资金费是聚合值，
//! 定位不到 symbol；Hyperliquid 的 WS 推送没有可去重的 ID。所别只体现在 `client.exchange()`
//! 与各 client 的 `fetch_funding_fees` 实现里。
//!
//! 设计：
//! - 每隔 `interval_ms` 拉一次，窗口固定为最近 24h
//! - 不做去重，下游凭 `FundingFee::tran_id` 自行去重（重叠窗口是有意的：
//!   宁可重复推送靠下游去重，也不要因为窗口对不齐而漏掉一次结算）

use crate::domain::now_ms;
use crate::exchange::FundingFeeSource;
use crate::messaging::{AccountBus, AccountData, AccountEvent, SymbolAccountData};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::sync::Arc;
use std::time::Duration;

/// 资费拉取窗口（24 小时）
const FUNDING_FEE_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1000;

/// 定时拉取本账户资金费流水
pub struct FundingFeePoller {
    client: Arc<dyn FundingFeeSource>,
    account_bus: AccountBus,
}

impl FundingFeePoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）。所别取自 `client.exchange()`。
    pub fn spawn(
        group: &Group,
        client: Arc<dyn FundingFeeSource>,
        account_bus: AccountBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        let exchange = client.exchange();
        tracing::info!(%exchange, interval_ms, "FundingFeePoller started");
        group.spawn_periodic(
            format!("{exchange}FundingFeePoller"),
            Duration::from_millis(interval_ms),
            Self { client, account_bus },
        )
    }
}

impl Periodic for FundingFeePoller {
    async fn tick(&mut self) -> anyhow::Result<()> {
        let now = now_ms();
        let start = now.saturating_sub(FUNDING_FEE_LOOKBACK_MS);

        match self.client.fetch_funding_fees(start, now).await {
            Ok(fees) => {
                let local_ts = now_ms();
                for fee in fees {
                    let exchange_ts = fee.timestamp;
                    self.account_bus.publish(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts,
                        local_ts,
                        data: AccountData::Symbol(SymbolAccountData::FundingFee(fee)),
                    });
                }
            }
            // 不接 StalenessGuard（与账户净值/希腊值轮询不同）：资费是**流水累加**而非
            // 快照读数，取不到就是这段区间的明细还没入账 —— 消费方看到的是"少了几条"，
            // 不是"一个假的当前值"，也不参与任何交易决策（只进核算）。按"能靠 fail-fast
            // 兜底的就不必层层设防"，这里保持重试。
            Err(e) => {
                tracing::warn!(
                    exchange = %self.client.exchange(),
                    error = %e,
                    "Failed to fetch funding fees"
                );
            }
        }
        Ok(())
    }
}

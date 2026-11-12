//! HyperliquidFundingFeePoller - 定时拉取 Hyperliquid 资费历史
//!
//! 与 `BinanceFundingFeePoller` 对位，行为刻意保持一致：
//! - 每隔 `interval_ms` 拉一次，窗口固定为最近 24h
//! - 不做去重，下游凭 `FundingFee::tran_id` 自行去重（重叠窗口是有意的：
//!   宁可重复推送靠下游去重，也不要因为窗口对不齐而漏掉一次结算）
//!
//! 为什么走 REST 而不是 WS `userFundings` 订阅：`FundingFee` 需要一个稳定的去重 ID，
//! 而 WS 推送不带任何 ID，且重连时会重推整份 snapshot。REST 这条同样不带可用 ID
//! （`hash` 恒为全零），但至少能由 `(time, coin)` 稳定派生，且拉取窗口可控。

use crate::domain::now_ms;
use crate::exchange::hyperliquid::HyperliquidClient;
use crate::messaging::{AccountBus, AccountData, AccountEvent, SymbolAccountData};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::sync::Arc;
use std::time::Duration;

/// 资费拉取窗口（24 小时）
const FUNDING_FEE_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1000;

pub(super) struct HyperliquidFundingFeePoller {
    client: Arc<HyperliquidClient>,
    account_bus: AccountBus,
}

impl HyperliquidFundingFeePoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）
    pub(super) fn spawn(
        group: &Group,
        client: Arc<HyperliquidClient>,
        account_bus: AccountBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!(exchange = "Hyperliquid", interval_ms, "HyperliquidFundingFeePoller started");
        group.spawn_periodic(
            "HyperliquidFundingFeePoller",
            Duration::from_millis(interval_ms),
            Self { client, account_bus },
        )
    }
}

impl Periodic for HyperliquidFundingFeePoller {
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
                    exchange = "Hyperliquid",
                    error = %e,
                    "Failed to fetch funding fees"
                );
            }
        }
        Ok(())
    }
}

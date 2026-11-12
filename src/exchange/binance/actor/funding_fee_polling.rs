//! BinanceFundingFeePoller - 定时拉取 Binance 资费历史
//!
//! 全仓模式下 WS 推送的 ACCOUNT_UPDATE.bc 是聚合值无法定位 symbol，
//! 通过 REST `GET /fapi/v1/income?incomeType=FUNDING_FEE` 获取 per-symbol 明细。
//!
//! 设计：
//! - 每隔 `interval_ms` 拉取一次，窗口固定为最近 24h
//! - 不做去重，下游凭 `FundingFee::tran_id` 自行去重

use crate::domain::now_ms;
use crate::exchange::binance::BinanceClient;
use crate::messaging::{AccountBus, AccountData, AccountEvent, SymbolAccountData};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::sync::Arc;
use std::time::Duration;

/// 资费拉取窗口（24 小时）
const FUNDING_FEE_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1000;

pub(super) struct BinanceFundingFeePoller {
    client: Arc<BinanceClient>,
    account_bus: AccountBus,
}

impl BinanceFundingFeePoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）
    pub(super) fn spawn(
        group: &Group,
        client: Arc<BinanceClient>,
        account_bus: AccountBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!(exchange = "Binance", interval_ms, "BinanceFundingFeePoller started");
        group.spawn_periodic(
            "BinanceFundingFeePoller",
            Duration::from_millis(interval_ms),
            Self { client, account_bus },
        )
    }
}

impl Periodic for BinanceFundingFeePoller {
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
                    exchange = "Binance",
                    error = %e,
                    "Failed to fetch funding fees"
                );
            }
        }
        Ok(())
    }
}

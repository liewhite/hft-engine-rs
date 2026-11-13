//! FundingFeePoller - 定时用 REST 拉账户最近 24h 的资金费结算明细，逐笔发布到账户总线
//!
//! 各所通用（所别只体现在 [`FundingFeeSource`] 的实现里）：
//! - 每隔 `interval_ms` 拉一次，窗口固定为最近 24h
//! - 不做去重，下游凭 `FundingFee::tran_id` 自行去重（重叠窗口是有意的：
//!   宁可重复推送靠下游去重，也不要因为窗口对不齐而漏掉一次结算）
//!
//! 不接 StalenessGuard（与账户净值/希腊值轮询不同）：资费是**流水**而非快照读数，取不到
//! 就是这段区间的明细还没入账 —— 消费方看到的是"少了几条"，不是"一个假的当前值"，
//! 也不参与任何交易决策（只进核算与观测）。失败记 warn，下一拍重试。

use crate::domain::{now_ms, AccountId};
use crate::exchange::FundingFeeSource;
use crate::messaging::{AccountBus, AccountData, AccountEvent, SymbolAccountData};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::sync::Arc;
use std::time::Duration;

/// 资费拉取窗口（24 小时）
const FUNDING_FEE_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1000;

/// 资费轮询间隔：结算最密的所（HL）每小时一次，一分钟一轮足够
pub(crate) const FUNDING_FEE_POLL_INTERVAL_MS: u64 = 60_000;

pub(crate) struct FundingFeePoller {
    source: Arc<dyn FundingFeeSource>,
    account_bus: AccountBus,
}

impl FundingFeePoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）。所别取自 `source.exchange()`。
    pub(crate) fn spawn(
        group: &Group,
        source: Arc<dyn FundingFeeSource>,
        account_bus: AccountBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        let exchange = source.exchange();
        tracing::info!(%exchange, interval_ms, "FundingFeePoller started");
        group.spawn_periodic(
            format!("{exchange}FundingFeePoller"),
            Duration::from_millis(interval_ms),
            Self {
                source,
                account_bus,
            },
        )
    }
}

impl Periodic for FundingFeePoller {
    async fn tick(&mut self) -> anyhow::Result<()> {
        let now = now_ms();
        let start = now.saturating_sub(FUNDING_FEE_LOOKBACK_MS);
        match self.source.fetch_funding_fees(start, now).await {
            Ok(fees) => {
                let local_ts = now_ms();
                for fee in fees {
                    self.account_bus.publish(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: AccountId::Live,
                        exchange_ts: fee.timestamp,
                        local_ts,
                        data: AccountData::Symbol(SymbolAccountData::FundingFee(fee)),
                    });
                }
            }
            Err(e) => {
                tracing::warn!(exchange = %self.source.exchange(), error = %e, "Failed to fetch funding fees");
            }
        }
        Ok(())
    }
}

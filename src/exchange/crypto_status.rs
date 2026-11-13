//! CryptoStatus - 加密货币交易所市场状态广播
//!
//! 加密货币交易所 7x24 运行，当前始终为 Liquid 状态。
//! 定时发布 ExchangeStatus 事件，供策略层统一感知市场状态。
//!
//! 保留独立组件的理由：
//! - StateManager 默认 Closed（安全侧），需要主动"喂"状态以避免策略误判
//! - 未来加密交易所可能有维护停机等非 Liquid 状态，届时可在此扩展判定逻辑
//!
//! # 为什么住在适配层
//!
//! "这个场子什么时候开门"是**场子的属性**，不是引擎的编排逻辑 —— 加密所 7x24、
//! IBKR 有交易时段（它自己走 `status_polling` 查交易日历）。本组件只由三个加密所的
//! 适配层 spawn，此前却住在 `engine::live`，于是适配层为了 spawn 它反向依赖引擎层
//! （`docs/architecture.md` V9）。搬到本层后那条边消失，且"新增一个 7x24 的所就复用它、
//! 新增一个有交易时段的所就自己写"这件事，在目录上也看得出来了。

use crate::domain::{now_ms, Exchange, MarketStatus};
use crate::messaging::{MarketBus, MarketData, MarketEvent};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::time::Duration;

/// 加密货币市场状态广播器：定时发布 Liquid
pub struct CryptoStatus {
    exchange: Exchange,
    market_bus: MarketBus,
}

impl CryptoStatus {
    /// 在 `group` 下起定时广播，每 `interval_ms` 毫秒一次（首次立即）
    pub fn spawn(
        group: &Group,
        exchange: Exchange,
        market_bus: MarketBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!(%exchange, "CryptoStatus started");
        group.spawn_periodic(
            format!("{exchange}CryptoStatus"),
            Duration::from_millis(interval_ms),
            Self {
                exchange,
                market_bus,
            },
        )
    }
}

impl Periodic for CryptoStatus {
    async fn tick(&mut self) -> anyhow::Result<()> {
        let local_ts = now_ms();
        self.market_bus.publish(MarketEvent {
            exchange_ts: local_ts,
            local_ts,
            data: MarketData::ExchangeStatus {
                exchange: self.exchange,
                status: MarketStatus::Liquid,
            },
        });
        Ok(())
    }
}

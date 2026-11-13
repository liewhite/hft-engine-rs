//! Clock - 定时广播时钟信号
//!
//! 每隔 `interval_ms` 往行情总线发一条 `MarketData::Clock`（首拍立即）。策略的定时逻辑、
//! 本地柜台的净值发布都由它驱动。

use crate::domain::now_ms;
use crate::messaging::{MarketBus, MarketData, MarketEvent};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use std::time::Duration;

/// 时钟信号广播器
pub struct Clock {
    market_bus: MarketBus,
}

impl Clock {
    /// 在 `group` 下起时钟，每 `interval_ms` 毫秒往 `market_bus` 发一条 Clock 事件。
    pub fn spawn(
        group: &Group,
        interval_ms: u64,
        market_bus: MarketBus,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!(interval_ms, "Clock started");
        group.spawn_periodic(
            "Clock",
            Duration::from_millis(interval_ms),
            Self { market_bus },
        )
    }
}

impl Periodic for Clock {
    async fn tick(&mut self) -> anyhow::Result<()> {
        let local_ts = now_ms();
        self.market_bus.publish(MarketEvent {
            exchange_ts: local_ts,
            local_ts,
            data: MarketData::Clock,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{Inbox, Root, Scope, Task};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counter(Inbox<MarketEvent>, Arc<AtomicUsize>);
    impl Task for Counter {
        async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
            while let Some(ev) = self.0.recv().await {
                assert!(matches!(ev.data, MarketData::Clock));
                self.1.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    /// 首拍立即，之后按周期发到行情总线
    #[tokio::test(start_paused = true)]
    async fn publishes_clock_events_on_schedule() {
        let root = Root::new("t");
        let bus = MarketBus::new("market");
        let n = Arc::new(AtomicUsize::new(0));
        let producers = root.group("producers");
        let consumers = root.group("consumers");
        let (b, c) = (bus.clone(), n.clone());
        consumers
            .spawn("counter", move |scope| async move {
                let (_addr, mut inbox) = scope.inbox();
                inbox.subscribe(&b, |_| true, |e| e);
                Ok(Counter(inbox, c))
            })
            .await
            .unwrap();
        Clock::spawn(&producers, 1000, bus).unwrap();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(n.load(Ordering::SeqCst), 3, "0s、1s、2s 各一拍");
        root.shutdown().await.unwrap();
    }
}

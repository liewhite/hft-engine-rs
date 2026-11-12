//! 事件总线：一对多扇出，在发布者自己的任务里同步投递。
//!
//! - 订阅返回 [`Subscription`]，**它被 drop 就退订**。订阅者把它存在自己的状态里，订阅的
//!   生命期就等于订阅者任务的生命期，不需要退订原语，也不会有"订阅者死了还挂在总线上"。
//! - 过滤器是 `fn` 指针，只看事件本身：不通过的事件连 clone 都省掉。
//! - 投递进订阅者的无界收件箱，不会因为对方慢而丢。投递失败只可能是订阅者已退出而
//!   `Subscription` 还没来得及 drop（退出瞬间的竞态），或订阅者把 `Subscription` 交给了别人
//!   （用法错误）：记 ERROR 并摘除，不静默。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use super::Gone;

type Sink<E> = Box<dyn Fn(E) -> Result<(), Gone> + Send + Sync>;

struct Subscriber<E> {
    id: u64,
    name: &'static str,
    filter: fn(&E) -> bool,
    sink: Sink<E>,
}

/// 一条总线。克隆得到的是同一条总线。
pub struct Bus<E> {
    name: &'static str,
    subscribers: Arc<RwLock<Vec<Subscriber<E>>>>,
}

impl<E> Clone for Bus<E> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            subscribers: self.subscribers.clone(),
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl<E: Clone + Send + Sync + 'static> Bus<E> {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            subscribers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// 订阅：`filter` 通过的事件投给 `sink`（通常是 [`super::Addr::sink`]）。
    /// 返回的 [`Subscription`] 被 drop 即退订。
    #[must_use = "Subscription 被 drop 就会退订"]
    pub fn subscribe(
        &self,
        name: &'static str,
        filter: fn(&E) -> bool,
        sink: impl Fn(E) -> Result<(), Gone> + Send + Sync + 'static,
    ) -> Subscription {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        self.subscribers.write().unwrap().push(Subscriber {
            id,
            name,
            filter,
            sink: Box::new(sink),
        });
        let weak: Weak<RwLock<Vec<Subscriber<E>>>> = Arc::downgrade(&self.subscribers);
        Subscription {
            unsubscribe: Some(Box::new(move || {
                if let Some(subs) = weak.upgrade() {
                    subs.write().unwrap().retain(|s| s.id != id);
                }
            })),
        }
    }

    /// 发布一条事件给所有过滤通过的订阅者
    pub fn publish(&self, event: E) {
        let mut gone = Vec::new();
        {
            let subs = self.subscribers.read().unwrap();
            for s in subs.iter().filter(|s| (s.filter)(&event)) {
                if (s.sink)(event.clone()).is_err() {
                    gone.push(s.id);
                    tracing::error!(bus = self.name, subscriber = s.name, "订阅者已退出但订阅仍在，摘除");
                }
            }
        }
        if !gone.is_empty() {
            self.subscribers.write().unwrap().retain(|s| !gone.contains(&s.id));
        }
    }

    /// 当前订阅者个数（诊断与测试用）
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.read().unwrap().len()
    }
}

/// 订阅凭证：drop 即退订
pub struct Subscription {
    unsubscribe: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(f) = self.unsubscribe.take() {
            f();
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Subscription")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::inbox;

    #[test]
    fn filter_delivery_and_unsubscribe_on_drop() {
        let bus = Bus::<u32>::new("t");
        let (even_addr, mut even_rx) = inbox::<u32>();
        let (all_addr, mut all_rx) = inbox::<u32>();
        let even = bus.subscribe("even", |n| n % 2 == 0, even_addr.sink(|n| n));
        let _all = bus.subscribe("all", |_| true, all_addr.sink(|n| n));

        for n in 0..4 {
            bus.publish(n);
        }
        drop(even);
        bus.publish(10);

        let drain = |rx: &mut crate::tasks::Inbox<u32>| {
            let root = crate::tasks::Scope::root("t");
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let s = root.clone();
                tokio::spawn(async move { s.shutdown().await });
                root.stopping().await;
                let mut v = Vec::new();
                while let Some(n) = rx.recv(&root).await {
                    v.push(n);
                }
                v
            })
        };
        assert_eq!(drain(&mut even_rx), [0, 2]);
        assert_eq!(drain(&mut all_rx), [0, 1, 2, 3, 10]);
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[test]
    fn dead_subscriber_is_removed() {
        let bus = Bus::<u32>::new("t");
        let (addr, rx) = inbox::<u32>();
        let sub = bus.subscribe("dead", |_| true, addr.sink(|n| n));
        drop(rx);
        bus.publish(1);
        assert_eq!(bus.subscriber_count(), 0);
        drop(sub);
    }
}

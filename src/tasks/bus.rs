//! 事件总线：一对多扇出，在发布者自己的任务里同步投递。
//!
//! - 只能经 [`super::Inbox::subscribe`] 订阅：订阅凭证存在订阅者的收件箱里，收件箱关闭
//!   （停机）即退订。能订阅的一定是挂在任务树上的组件。
//! - 过滤器是 `fn` 指针，只看事件本身：不通过的事件连 clone 都省掉。
//! - 投递进订阅者的无界收件箱，不会因为对方慢而丢。订阅者停机时收件箱先关闭、再退订，
//!   两步之间到达的投递失败：记 ERROR「事件丢失」并摘除，不静默。
//! - 发布与订阅是两种能力：[`Bus`] 能发能订，[`BusTap`]（[`Bus::tap`]）只能订。交给外部
//!   观察者的一律是 `BusTap` —— 观察者往引擎总线里塞事件在类型上不可表达。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use super::Gone;

type Sink<E> = Box<dyn Fn(E) -> Result<(), Gone> + Send + Sync>;

struct Subscriber<E> {
    id: u64,
    name: String,
    filter: fn(&E) -> bool,
    sink: Sink<E>,
}

/// 一条总线：能发布、能订阅。克隆得到的是同一条总线。
pub struct Bus<E> {
    tap: BusTap<E>,
}

/// 同一条总线的只订阅视图（见模块文档）：没有 `publish`。克隆得到的是同一条总线。
///
/// ```compile_fail
/// let bus = hft_engine_rs::tasks::Bus::<u32>::new("t");
/// bus.tap().publish(1); // 只订阅视图不能发布
/// ```
pub struct BusTap<E> {
    name: &'static str,
    subscribers: Arc<RwLock<Vec<Subscriber<E>>>>,
}

impl<E> Clone for Bus<E> {
    fn clone(&self) -> Self {
        Self { tap: self.tap.clone() }
    }
}

impl<E> Clone for BusTap<E> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            subscribers: self.subscribers.clone(),
        }
    }
}

/// 订阅只要视图：[`super::Inbox::subscribe`] 收 `&Bus` 或 `&BusTap` 都行
impl<E> AsRef<BusTap<E>> for Bus<E> {
    fn as_ref(&self) -> &BusTap<E> {
        &self.tap
    }
}

impl<E> AsRef<BusTap<E>> for BusTap<E> {
    fn as_ref(&self) -> &BusTap<E> {
        self
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl<E: Clone + Send + Sync + 'static> Bus<E> {
    pub fn new(name: &'static str) -> Self {
        Self {
            tap: BusTap {
                name,
                subscribers: Arc::new(RwLock::new(Vec::new())),
            },
        }
    }

    /// 只能订阅的视图，交给不该发布的一方（外部观察者）
    pub fn tap(&self) -> BusTap<E> {
        self.tap.clone()
    }

    /// 发布一条事件给所有过滤通过的订阅者
    pub fn publish(&self, event: E) {
        let tap = &self.tap;
        let mut gone = Vec::new();
        {
            let subs = tap.subscribers.read().unwrap();
            for s in subs.iter().filter(|s| (s.filter)(&event)) {
                if (s.sink)(event.clone()).is_err() {
                    gone.push(s.id);
                    tracing::error!(bus = tap.name, subscriber = %s.name, "投递失败，事件丢失（订阅者已停止收件却仍在订阅），摘除");
                }
            }
        }
        if !gone.is_empty() {
            tap.subscribers.write().unwrap().retain(|s| !gone.contains(&s.id));
        }
    }

    /// 当前订阅者个数（诊断与测试用）
    pub fn subscriber_count(&self) -> usize {
        self.tap.subscribers.read().unwrap().len()
    }
}

impl<E: Clone + Send + Sync + 'static> BusTap<E> {
    pub(super) fn subscribe(
        &self,
        name: String,
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
}

/// 订阅凭证：drop 即退订（存在订阅者的收件箱里，见 [`super::Inbox::subscribe`]）
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
    use crate::tasks::Root;
    use tokio::sync::oneshot;

    /// 过滤、投递，以及订阅者停机即退订
    #[tokio::test]
    async fn filter_delivery_and_unsubscribe_on_stop() {
        let bus = Bus::<u32>::new("t");
        let root = Root::new("t");
        let (ready_tx, ready_rx) = oneshot::channel();
        let (got_tx, got_rx) = oneshot::channel();
        let b = bus.clone();
        let h = root
            .group("g")
            .spawn_fn("even", move |scope| async move {
                let (_addr, mut inbox) = scope.inbox::<u32>();
                inbox.subscribe(&b, |n| n % 2 == 0, |n| n);
                ready_tx.send(()).unwrap();
                let mut got = Vec::new();
                while let Some(n) = inbox.recv().await {
                    got.push(n);
                }
                got_tx.send(got).unwrap();
                Ok(())
            })
            .unwrap();
        ready_rx.await.unwrap();
        assert_eq!(bus.subscriber_count(), 1);
        for n in 0..5 {
            bus.publish(n);
        }
        h.stop().await;
        assert_eq!(got_rx.await.unwrap(), [0, 2, 4]);
        assert_eq!(bus.subscriber_count(), 0, "停机即退订");
        bus.publish(6); // 无订阅者：不报错
        root.shutdown().await.unwrap();
    }

    /// 经只订阅视图订阅：收到的是同一条总线上发布的事件，退订也照常
    #[tokio::test]
    async fn tap_subscribes_to_the_same_bus() {
        let bus = Bus::<u32>::new("t");
        let tap = bus.tap();
        let root = Root::new("t");
        let (ready_tx, ready_rx) = oneshot::channel();
        let (got_tx, got_rx) = oneshot::channel();
        let h = root
            .group("g")
            .spawn_fn("observer", move |scope| async move {
                let (_addr, mut inbox) = scope.inbox::<u32>();
                inbox.subscribe(&tap, |_| true, |n| n);
                ready_tx.send(()).unwrap();
                let mut got = Vec::new();
                while let Some(n) = inbox.recv().await {
                    got.push(n);
                }
                got_tx.send(got).unwrap();
                Ok(())
            })
            .unwrap();
        ready_rx.await.unwrap();
        assert_eq!(bus.subscriber_count(), 1);
        bus.publish(1);
        bus.publish(2);
        h.stop().await;
        assert_eq!(got_rx.await.unwrap(), [1, 2]);
        assert_eq!(bus.subscriber_count(), 0);
        root.shutdown().await.unwrap();
    }
}

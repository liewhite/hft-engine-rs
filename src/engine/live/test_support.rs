//! 引擎层组件测试共用：把总线上的事件收进一个 Vec 供断言。

use crate::tasks::{Bus, Group, Inbox, Scope, Task};
use std::sync::{Arc, Mutex};

struct Collector<E> {
    inbox: Inbox<E>,
    out: Arc<Mutex<Vec<E>>>,
}

impl<E: Send + 'static> Task for Collector<E> {
    async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
        while let Some(e) = self.inbox.recv().await {
            self.out.lock().unwrap().push(e);
        }
        Ok(())
    }
}

/// 在 `group` 下起一个订阅 `bus` 全量事件的收集器，返回收到的事件（订阅完成后才返回）
pub(crate) async fn collect<E: Clone + Send + Sync + 'static>(
    group: &Group,
    bus: &Bus<E>,
) -> Arc<Mutex<Vec<E>>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let (bus, sink) = (bus.clone(), out.clone());
    group
        .spawn("collector", move |scope| async move {
            let (_addr, mut inbox) = scope.inbox::<E>();
            inbox.subscribe(&bus, |_| true, |e| e);
            Ok(Collector { inbox, out: sink })
        })
        .await
        .expect("收集器起不来");
    out
}

//! 两种反复出现的组件形状的便捷入口，都只是 [`Group::spawn`] / [`Group::spawn_fn`] 的薄包装：
//!
//! - [`Group::spawn_addressed`]：起一个有收件箱的组件，并把它的地址交给调用方。几乎每个
//!   "父节点要给子节点发消息"的地方都要这一步（交易所节点、WS 子任务、executor、各处理器），
//!   不收拢的话每处都要自己拿 oneshot 把地址从构造闭包里带出来。
//! - [`Group::spawn_periodic`]：定时轮询。迁移前有十来个"每隔 N 毫秒查一次 REST"的 actor，
//!   形状完全相同：节拍 → 做一次 → 出错整支停。
//!
//! 节拍用 [`MissedTickBehavior::Skip`]：一次轮询慢于周期时不积压补跑（旧版 kameo 的定时流
//! 不等处理完就投递，曾要专门丢弃邮箱里积压的过时拍）。首拍立即触发。

use std::future::Future;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

use super::{Addr, ChildHandle, Group, GroupClosed, Inbox, Scope, Task};

/// 定时执行的一件事，见 [`Group::spawn_periodic`]
pub trait Periodic: Send + 'static {
    /// 做一次。返回 `Err` = 整支停（错误上冒）；"记日志继续"的情况在实现里自己处理后返回 `Ok`。
    fn tick(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

impl Group {
    /// 起一个有收件箱的组件，等它构造完成，返回句柄和它的地址。
    ///
    /// `build` 拿到子任务的 [`Scope`] 和收件箱（在里面订阅总线、起孙任务），返回组件本身。
    /// 失败语义同 [`Group::spawn`]。
    pub async fn spawn_addressed<M, T, B, Fut>(
        &self,
        name: impl Into<String>,
        build: B,
    ) -> anyhow::Result<(ChildHandle, Addr<M>)>
    where
        M: Send + 'static,
        T: Task,
        B: FnOnce(Scope, Inbox<M>) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let name = name.into();
        let (addr_tx, addr_rx) = oneshot::channel();
        let handle = self
            .spawn(name.clone(), move |scope| async move {
                let (addr, inbox) = scope.inbox::<M>();
                // 接收端在本函数里等着，构造成功前不会被丢弃
                let _ = addr_tx.send(addr);
                build(scope, inbox).await
            })
            .await?;
        // 构造成功说明闭包已跑过上面的 send
        let addr = addr_rx
            .await
            .map_err(|_| anyhow::anyhow!("{name} 构造成功却没有交出地址（不该发生）"))?;
        Ok((handle, addr))
    }

    /// 起一个定时任务：每隔 `every` 调一次 [`Periodic::tick`]（首拍立即），被要求停止时
    /// 在两拍之间退出（正在做的那一次一定做完）。
    pub fn spawn_periodic<P: Periodic>(
        &self,
        name: impl Into<String>,
        every: Duration,
        mut job: P,
    ) -> Result<ChildHandle, GroupClosed> {
        self.spawn_fn(name, move |scope| async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    // 停止优先：已请求停止时不再开始新的一次
                    biased;
                    _ = scope.stopping() => return Ok(()),
                    _ = tick.tick() => job.tick().await?,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::Root;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counter(Arc<AtomicUsize>, usize);

    impl Periodic for Counter {
        async fn tick(&mut self) -> anyhow::Result<()> {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            anyhow::ensure!(n < self.1, "第 {n} 次失败");
            Ok(())
        }
    }

    /// 首拍立即、按周期跑；正常停机不算错
    #[tokio::test(start_paused = true)]
    async fn periodic_ticks_and_stops_cleanly() {
        let n = Arc::new(AtomicUsize::new(0));
        let root = Root::new("t");
        root.group("g")
            .spawn_periodic("p", Duration::from_secs(1), Counter(n.clone(), usize::MAX))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(n.load(Ordering::SeqCst), 3, "0s、1s、2s 各一次");
        root.shutdown().await.unwrap();
    }

    /// 一次出错：整支停、错误冒到根
    #[tokio::test(start_paused = true)]
    async fn periodic_error_propagates() {
        let root = Root::new("t");
        root.group("g")
            .spawn_periodic("p", Duration::from_secs(1), Counter(Arc::new(AtomicUsize::new(0)), 2))
            .unwrap();
        root.stopping().await;
        let e = root.shutdown().await.unwrap_err();
        assert!(format!("{e:#}").contains("第 2 次失败"), "{e:#}");
    }

    /// 地址在构造完成时交到调用方手里，消息送达
    #[tokio::test]
    async fn spawn_addressed_hands_back_the_address() {
        struct Echo(Inbox<oneshot::Sender<u32>>);
        impl Task for Echo {
            async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
                while let Some(reply) = self.0.recv().await {
                    let _ = reply.send(42);
                }
                Ok(())
            }
        }
        let root = Root::new("t");
        let (_h, addr) = root
            .group("g")
            .spawn_addressed("echo", |_scope, inbox| async move { Ok(Echo(inbox)) })
            .await
            .unwrap();
        assert_eq!(addr.ask(|tx| tx).await.unwrap(), 42);
        root.shutdown().await.unwrap();
    }

    /// 构造失败照常返回 Err
    #[tokio::test]
    async fn spawn_addressed_build_failure_is_an_error() {
        struct Never;
        impl Task for Never {
            async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let root = Root::new("t");
        let r = root
            .group("g")
            .spawn_addressed::<u32, Never, _, _>("bad", |_s, _i| async move { anyhow::bail!("连不上") })
            .await;
        assert!(format!("{:#}", r.unwrap_err()).contains("连不上"));
        assert!(root.shutdown().await.is_err());
    }
}

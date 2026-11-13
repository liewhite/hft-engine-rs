//! [`Group::spawn_periodic`]：定时轮询，[`Group::spawn_fn`] 的薄包装。迁移前有十来个"每隔
//! N 毫秒查一次 REST"的 actor，形状完全相同：节拍 → 做一次 → 出错整支停。
//!
//! 节拍用 [`MissedTickBehavior::Skip`]：一次轮询慢于周期时不积压补跑 —— 错过的几拍合并成
//! 做完时立即的一拍，之后回到整周期节奏（旧版 kameo 的定时流不等处理完就投递，曾要专门
//! 丢弃邮箱里积压的过时拍）。首拍立即触发。

use std::future::Future;
use std::time::Duration;
use tokio::time::MissedTickBehavior;

use super::{ChildHandle, Group, GroupClosed};

/// 定时执行的一件事，见 [`Group::spawn_periodic`]
pub trait Periodic: Send + 'static {
    /// 做一次。返回 `Err` = 整支停（错误上冒）；"记日志继续"的情况在实现里自己处理后返回 `Ok`。
    fn tick(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

impl Group {
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
            .spawn_periodic(
                "p",
                Duration::from_secs(1),
                Counter(Arc::new(AtomicUsize::new(0)), 2),
            )
            .unwrap();
        root.stopping().await;
        let e = root.shutdown().await.unwrap_err();
        assert!(format!("{e:#}").contains("第 2 次失败"), "{e:#}");
    }

    /// 记下每一拍开始的时刻（相对起点）；第一拍耗时 `first_takes`，之后瞬间完成
    struct Slow {
        start: tokio::time::Instant,
        first_takes: Duration,
        starts: Arc<std::sync::Mutex<Vec<Duration>>>,
        finished: Arc<AtomicUsize>,
    }

    impl Periodic for Slow {
        async fn tick(&mut self) -> anyhow::Result<()> {
            let n = {
                let mut starts = self.starts.lock().unwrap();
                starts.push(self.start.elapsed());
                starts.len()
            };
            if n == 1 {
                tokio::time::sleep(self.first_takes).await;
            }
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn slow(
        first_takes: Duration,
    ) -> (Slow, Arc<std::sync::Mutex<Vec<Duration>>>, Arc<AtomicUsize>) {
        let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let finished = Arc::new(AtomicUsize::new(0));
        let job = Slow {
            start: tokio::time::Instant::now(),
            first_takes,
            starts: starts.clone(),
            finished: finished.clone(),
        };
        (job, starts, finished)
    }

    /// 一拍慢于周期时不补跑：错过的几拍合并成做完时立即的一拍，之后回到整周期节奏
    /// （不会像 Burst 那样按错过的拍数连跑）
    #[tokio::test(start_paused = true)]
    async fn periodic_skips_missed_ticks() {
        let (job, starts, _) = slow(Duration::from_millis(2500));
        let root = Root::new("t");
        root.group("g")
            .spawn_periodic("p", Duration::from_secs(1), job)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let ms: Vec<u128> = starts
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.as_millis())
            .collect();
        // 0s 那拍做到 2.5s；错过的 1s、2s 两拍只在 2.5s 补一拍（Burst 会在 2.5s 连跑两拍），
        // 下一拍回到整周期 3s
        assert_eq!(ms, [0, 2500, 3000]);
        root.shutdown().await.unwrap();
    }

    /// 停止不打断正在做的那一拍：做完才退出，之后不再开新的一拍
    #[tokio::test(start_paused = true)]
    async fn stop_waits_for_the_running_tick() {
        let (job, starts, finished) = slow(Duration::from_secs(2));
        let root = Root::new("t");
        let h = root
            .group("g")
            .spawn_periodic("p", Duration::from_secs(1), job)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let t0 = tokio::time::Instant::now();
        h.stop().await;
        assert!(
            t0.elapsed() >= Duration::from_millis(1500),
            "停止应等这一拍做完"
        );
        assert_eq!(finished.load(Ordering::SeqCst), 1, "正在做的一拍被打断了");
        assert_eq!(starts.lock().unwrap().len(), 1, "停止后不应再开新的一拍");
        root.shutdown().await.unwrap();
    }
}

//! 组件的收件箱：无界通道 + 停机时关闭并排空。只能经 [`Scope::inbox`] 新建。
//!
//! # 收件箱的生死只跟着任务走
//!
//! [`Inbox`] 自己留着一个发送端，所以"所有地址都被丢掉"不会让它关闭 —— 组件不会因为没人
//! 持有它的地址而悄悄退出。组件何时退出只由任务树决定（停止信号或它自己返回）。
//!
//! # 停机时先关闭、再排空
//!
//! 收到停止信号后，[`Inbox::recv`] 先关闭接收端（此后的 [`Addr::send`] 和总线投递立刻失败、
//! 留下错误记录，而不是"发送成功"却没人取），再把已经进箱的消息交完，然后返回 `None`。
//! 配合任务树的组间顺序（先停生产者、后停消费者），生产者收尾时发出的最后一批消息一定会
//! 被处理。
//!
//! # 订阅跟着收件箱走
//!
//! [`Inbox::subscribe`] 把订阅凭证存在收件箱里：收件箱关闭即退订，订阅不会比订阅者活得久，
//! 也不需要组件记得把凭证存到哪里。

use tokio::sync::{mpsc, oneshot};

use super::{Bus, Scope, Subscription};

pub(super) fn new<M>(scope: Scope) -> (Addr<M>, Inbox<M>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Addr(tx.clone()),
        Inbox {
            rx,
            keep_open: Some(tx),
            scope,
            subscriptions: Vec::new(),
        },
    )
}

/// 收件箱的接收端，由组件自己持有
pub struct Inbox<M> {
    rx: mpsc::UnboundedReceiver<M>,
    /// 保证通道不会因发送端全部 drop 而关闭，见模块文档；停机时丢掉
    keep_open: Option<mpsc::UnboundedSender<M>>,
    /// 所属节点：停止信号从这里来
    scope: Scope,
    /// 本收件箱的总线订阅；关闭时一并退订
    subscriptions: Vec<Subscription>,
}

impl<M: Send + 'static> Inbox<M> {
    /// 取下一条消息；所属节点被请求停止后，关闭收件箱、交完已进箱的消息，然后返回 `None`。
    pub async fn recv(&mut self) -> Option<M> {
        if self.keep_open.is_some() {
            tokio::select! {
                // 有消息先处理：停止信号与消息同时就绪时，消息不会被跳过（反正之后也要排空）
                biased;
                msg = self.rx.recv() => return msg,
                _ = self.scope.stopping() => self.close(),
            }
        }
        // 已关闭：箱里剩下的照常取出，取完返回 None
        self.rx.recv().await
    }

    /// 订阅一条总线：`filter` 通过的事件经 `wrap` 包成本收件箱的消息投进来。
    /// 订阅随收件箱关闭（停机）或销毁而退订。
    pub fn subscribe<E: Clone + Send + Sync + 'static>(
        &mut self,
        bus: &Bus<E>,
        filter: fn(&E) -> bool,
        wrap: fn(E) -> M,
    ) {
        let tx = self
            .keep_open
            .clone()
            .expect("收件箱已关闭（节点在停机），不能再订阅");
        let name = self.scope.name().to_string();
        self.subscriptions.push(bus.subscribe(name, filter, move |e| {
            tx.send(wrap(e)).map_err(|_| super::Gone)
        }));
    }

    fn close(&mut self) {
        // 先关接收端、再退订：两步之间到达的投递会失败并在总线上记「事件丢失」，不会静默
        self.rx.close();
        self.keep_open = None;
        self.subscriptions.clear();
    }
}

impl<M> Drop for Inbox<M> {
    /// 组件没把箱里的消息取完就退出（比如 `run` 在停止分支里直接 break、没再调 `recv`）：
    /// 关闭并点数，有剩余就报 ERROR —— 丢了要说出来。
    fn drop(&mut self) {
        self.rx.close();
        let mut lost = 0usize;
        while self.rx.try_recv().is_ok() {
            lost += 1;
        }
        if lost > 0 {
            tracing::error!(node = %self.scope.name(), lost, "收件箱销毁时仍有未处理的消息，已丢失");
        }
    }
}

/// 收件箱的地址：可克隆，发往一个组件。
pub struct Addr<M>(mpsc::UnboundedSender<M>);

impl<M> Clone for Addr<M> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<M> std::fmt::Debug for Addr<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Addr")
    }
}

/// 对方已停止收件（在停机或已退出），消息没有送达
#[derive(Debug, thiserror::Error)]
#[error("目标组件已停止收件")]
pub struct Gone;

impl<M> Addr<M> {
    /// 发一条消息，不等处理
    pub fn send(&self, msg: M) -> Result<(), Gone> {
        self.0.send(msg).map_err(|_| Gone)
    }

    /// 发一条带回执的消息并等回复：`make` 把回执通道装进消息。
    ///
    /// 对方停止收件（或收尾时丢弃了这条消息）返回 [`Gone`]。
    pub async fn ask<R>(&self, make: impl FnOnce(oneshot::Sender<R>) -> M) -> Result<R, Gone> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        rx.await.map_err(|_| Gone)
    }

    /// 是否已停止收件
    pub fn is_gone(&self) -> bool {
        self.0.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::Root;
    use std::time::Duration;

    /// 停机后：先交完箱里的，之后的发送立刻失败（不会"发送成功"却没人取）
    #[tokio::test]
    async fn closes_then_drains_after_stop() {
        let root = Root::new("t");
        let (addr_tx, addr_rx) = oneshot::channel();
        let (got_tx, got_rx) = oneshot::channel();
        root.group("g")
            .spawn_fn("c", |scope| async move {
                let (addr, mut inbox) = scope.inbox::<u32>();
                addr.send(1).unwrap();
                addr.send(2).unwrap();
                addr_tx.send(addr.clone()).unwrap();
                scope.stopping().await;
                let mut got = Vec::new();
                while let Some(n) = inbox.recv().await {
                    got.push(n);
                }
                got.push(u32::from(addr.send(3).is_err()) * 100);
                got_tx.send(got).unwrap();
                Ok(())
            })
            .unwrap();
        let _addr = addr_rx.await.unwrap();
        root.shutdown().await.unwrap();
        assert_eq!(got_rx.await.unwrap(), [1, 2, 100]);
    }

    /// 地址全丢了也不会让收件箱结束：只有停止信号能让 recv 返回 None
    #[tokio::test(start_paused = true)]
    async fn dropping_all_addresses_does_not_end_the_inbox() {
        let root = Root::new("t");
        let (tx, rx) = oneshot::channel();
        root.group("g")
            .spawn_fn("c", |scope| async move {
                let (addr, mut inbox) = scope.inbox::<u32>();
                drop(addr);
                let r = tokio::time::timeout(Duration::from_secs(60), inbox.recv()).await;
                tx.send(r.is_err()).unwrap();
                scope.stopping().await;
                Ok(())
            })
            .unwrap();
        assert!(rx.await.unwrap(), "没有停止信号时 recv 应一直等");
        root.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ask_gets_the_reply_and_gone_after_exit() {
        let root = Root::new("t");
        let (addr_tx, addr_rx) = oneshot::channel();
        let h = root
            .group("g")
            .spawn_fn("server", |scope| async move {
                let (addr, mut inbox) = scope.inbox::<oneshot::Sender<u32>>();
                addr_tx.send(addr).unwrap();
                while let Some(reply) = inbox.recv().await {
                    let _ = reply.send(7);
                }
                Ok(())
            })
            .unwrap();
        let addr = addr_rx.await.unwrap();
        assert_eq!(addr.ask(|tx| tx).await.unwrap(), 7);
        h.stop().await;
        assert!(addr.is_gone());
        assert!(addr.ask(|tx| tx).await.is_err());
        root.shutdown().await.unwrap();
    }
}

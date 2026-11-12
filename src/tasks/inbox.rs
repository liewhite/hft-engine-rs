//! 组件的收件箱：无界通道 + 停机时排空。
//!
//! # 收件箱的生死只跟着任务走
//!
//! [`Inbox`] 自己留着一个发送端，所以"所有地址都被丢掉"不会让它关闭 —— 组件不会因为没人
//! 持有它的地址而悄悄退出。组件何时退出只由任务树决定（停止信号或它自己返回）。
//!
//! # 停机时排空
//!
//! 收到停止信号后，[`Inbox::recv`] 先把已经进箱的消息交完，再返回 `None`。配合任务树的
//! 组间顺序（先停生产者、后停消费者），生产者收尾时发出的最后一批消息一定会被处理。

use tokio::sync::{mpsc, oneshot};

use super::Scope;

/// 新建一个收件箱和它的地址
pub fn inbox<M>() -> (Addr<M>, Inbox<M>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Addr(tx.clone()), Inbox { rx, _keep_open: tx })
}

/// 收件箱的接收端，由组件自己持有
pub struct Inbox<M> {
    rx: mpsc::UnboundedReceiver<M>,
    /// 保证通道不会因发送端全部 drop 而关闭，见模块文档
    _keep_open: mpsc::UnboundedSender<M>,
}

impl<M> Inbox<M> {
    /// 取下一条消息；`scope` 被请求停止后，交完已进箱的消息就返回 `None`。
    pub async fn recv(&mut self, scope: &Scope) -> Option<M> {
        if !scope.is_stopping() {
            tokio::select! {
                // 有消息先处理：停止信号与消息同时就绪时，消息不会被跳过（反正之后也要排空）
                biased;
                msg = self.rx.recv() => return msg,
                _ = scope.stopping() => {}
            }
        }
        self.rx.try_recv().ok()
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

/// 对方已退出（收件箱已随任务销毁）
#[derive(Debug, thiserror::Error)]
#[error("目标组件已退出")]
pub struct Gone;

impl<M> Addr<M> {
    /// 发一条消息，不等处理
    pub fn send(&self, msg: M) -> Result<(), Gone> {
        self.0.send(msg).map_err(|_| Gone)
    }

    /// 发一条带回执的消息并等回复：`make` 把回执通道装进消息。
    ///
    /// 对方退出（或收尾时丢弃了这条消息）返回 [`Gone`]。
    pub async fn ask<R>(&self, make: impl FnOnce(oneshot::Sender<R>) -> M) -> Result<R, Gone> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        rx.await.map_err(|_| Gone)
    }

    /// 是否已退出
    pub fn is_gone(&self) -> bool {
        self.0.is_closed()
    }

    /// 把 `E` 类事件包成本收件箱的消息再投递：给总线订阅用
    /// （`bus.subscribe(name, filter, addr.sink(Msg::Market))`）。
    pub fn sink<E: 'static>(&self, wrap: fn(E) -> M) -> impl Fn(E) -> Result<(), Gone> + Send + Sync + 'static
    where
        M: Send + 'static,
    {
        let tx = self.0.clone();
        move |e| tx.send(wrap(e)).map_err(|_| Gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drains_after_stop_then_ends() {
        let root = Scope::root("t");
        let (addr, mut inbox) = inbox::<u32>();
        addr.send(1).unwrap();
        addr.send(2).unwrap();
        let s = root.clone();
        tokio::spawn(async move { s.shutdown().await });
        root.stopping().await;
        assert_eq!(inbox.recv(&root).await, Some(1));
        assert_eq!(inbox.recv(&root).await, Some(2));
        assert_eq!(inbox.recv(&root).await, None);
    }

    /// 地址全丢了也不会让收件箱结束：只有停止信号能让 recv 返回 None
    #[tokio::test(start_paused = true)]
    async fn dropping_all_addresses_does_not_end_the_inbox() {
        let root = Scope::root("t");
        let (addr, mut inbox) = inbox::<u32>();
        drop(addr);
        let r = tokio::time::timeout(std::time::Duration::from_secs(60), inbox.recv(&root)).await;
        assert!(r.is_err(), "没有停止信号时 recv 应一直等");
    }

    #[tokio::test]
    async fn ask_gets_the_reply_and_gone_after_exit() {
        let (addr, mut inbox) = inbox::<oneshot::Sender<u32>>();
        let root = Scope::root("t");
        let r2 = root.clone();
        let task = tokio::spawn(async move {
            let reply = inbox.recv(&r2).await.unwrap();
            reply.send(7).unwrap();
        });
        assert_eq!(addr.ask(|tx| tx).await.unwrap(), 7);
        task.await.unwrap();
        assert!(addr.is_gone());
        assert!(addr.ask(|tx| tx).await.is_err());
    }
}

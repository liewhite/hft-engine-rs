//! 按需建的公共行情连接（#757）：交易所节点下的每条公共 WS，在第一条订阅命令路由到它时才连。
//!
//! 为什么不在节点构造时全连上：一条用不到的连接（funding_arb 用不到币安 /market/ws、OKX 的
//! K 线连接）照样占资源、照样可能出错停机，却没有任何收益。哪条连接要不要，由策略的订阅决定，
//! 节点不必事先知道。
//!
//! 连接在节点处理订阅命令时建（`await` 建连，期间节点的后续订阅命令排队）：节点的收件箱只有
//! 订阅命令，排队不耽误别的事；按序处理也保证同一条连接上的订阅顺序不乱。建连失败照常上冒、
//! 整机停机（与以前「启动时连不上即启动失败」同一档，只是时机挪到第一次订阅）。

use crate::exchange::client::ExchangeCmd;
use crate::tasks::Addr;
use anyhow::Context as _;
use std::future::Future;
use std::pin::Pin;

type SpawnFuture = Pin<Box<dyn Future<Output = anyhow::Result<Addr<ExchangeCmd>>> + Send>>;

/// 一条按需建的连接：还没连时握着「怎么建」，连上后握着组件地址
pub(crate) struct LazyConn {
    name: &'static str,
    state: State,
}

enum State {
    Idle(Box<dyn FnOnce() -> SpawnFuture + Send>),
    Up(Addr<ExchangeCmd>),
    /// 建连失败后的状态：错误已上冒、整机在停，不再重试
    Failed,
}

impl LazyConn {
    /// `spawn`：在节点的子分组下起这条连接的组件并等它连好，返回命令地址
    pub(crate) fn new<F, Fut>(name: &'static str, spawn: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<Addr<ExchangeCmd>>> + Send + 'static,
    {
        Self { name, state: State::Idle(Box::new(move || Box::pin(spawn()))) }
    }

    /// 把订阅命令交给这条连接，第一次有订阅时先建连。没连过的连接上的退订直接忽略（没有可退的）。
    pub(crate) async fn send(&mut self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        if matches!(cmd, ExchangeCmd::Unsubscribe(_)) && matches!(self.state, State::Idle(_)) {
            return Ok(());
        }
        if let State::Idle(_) = self.state {
            let State::Idle(spawn) = std::mem::replace(&mut self.state, State::Failed) else { unreachable!() };
            tracing::info!(conn = self.name, "第一次有订阅路由到这条连接，开始连接");
            let addr = spawn().await.with_context(|| format!("{} 建连失败", self.name))?;
            self.state = State::Up(addr);
        }
        match &self.state {
            State::Up(addr) => addr
                .send(cmd)
                .with_context(|| format!("转发订阅命令失败：{} 已停止收件", self.name)),
            State::Failed => anyhow::bail!("{} 建连已失败，不再转发订阅", self.name),
            State::Idle(_) => unreachable!("上面已建连"),
        }
    }

    /// 是否已建连（测试与日志用）
    #[cfg(test)]
    pub(crate) fn is_up(&self) -> bool {
        matches!(self.state, State::Up(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::SubscriptionKind;
    use crate::tasks::{Inbox, Root, Scope, Task};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Sink(Inbox<ExchangeCmd>);
    impl Task for Sink {
        async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
            while self.0.recv().await.is_some() {}
            Ok(())
        }
    }

    fn bbo() -> SubscriptionKind {
        SubscriptionKind::BBO { symbol: "BTC".to_string() }
    }

    /// 没有订阅就不建连；第一条订阅才建，之后复用同一条；没连过时的退订不建连
    #[tokio::test]
    async fn connects_on_first_subscription_only() {
        let root = Root::new("t");
        let group = root.group("g");
        let spawned = Arc::new(AtomicUsize::new(0));
        let counter = spawned.clone();
        let mut conn = LazyConn::new("TestWs", move || async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let (_, addr) = group.spawn_addressed("Sink", |_s: Scope, inbox| async move { Ok(Sink(inbox)) }).await?;
            Ok(addr)
        });
        conn.send(ExchangeCmd::Unsubscribe(bbo())).await.unwrap();
        assert_eq!(spawned.load(Ordering::SeqCst), 0, "退订不该建连");
        assert!(!conn.is_up());
        conn.send(ExchangeCmd::Subscribe(bbo())).await.unwrap();
        conn.send(ExchangeCmd::Unsubscribe(bbo())).await.unwrap();
        assert_eq!(spawned.load(Ordering::SeqCst), 1);
        assert!(conn.is_up());
        root.shutdown().await.unwrap();
    }

    /// 建连失败上冒，之后不再重试
    #[tokio::test]
    async fn connect_failure_is_reported_and_not_retried() {
        let mut conn = LazyConn::new("TestWs", || async { anyhow::bail!("连不上") });
        let err = conn.send(ExchangeCmd::Subscribe(bbo())).await.unwrap_err();
        assert!(format!("{err:#}").contains("连不上"), "{err:#}");
        assert!(conn.send(ExchangeCmd::Subscribe(bbo())).await.is_err());
    }
}

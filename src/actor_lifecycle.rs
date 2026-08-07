//! Actor 生命周期的公共约定：一棵树、一条判据、一条停机链。
//!
//! # 三条规则
//!
//! - **树形**：每个 actor 用 [`ChildGroup::spawn`] 起自己的子 actor，根是 `ManagerActor`
//!   （无 link）。外部观察者经 [`crate::engine::spawn_supervised`] 挂到根上。
//! - **出错向上级联**：判据是 kameo 的 `ActorStopReason` —— `Normal` 放行，`Killed` /
//!   `Panicked` / `LinkDied` 一律 `Break`。这正是 kameo 的**默认** `on_link_died`，
//!   所以**不要重写它**：历史上重写过的版本都是"无条件 Break"，连主动退出也误判成事故。
//! - **停机向下逐层**：谁 spawn 的谁负责等 —— 每个持有子 actor 的 actor 在 `on_stop` 里
//!   调一次 [`ChildGroup::shutdown`]。
//!
//! # 为什么"等"这一程绕不开
//!
//! kameo 的 `link` 建立的是**兄弟**关系，父 actor 停止时子 actor 只收到一条通知，不会
//! 被带停；即便改用 kameo 的父子监督，它的 `wait_children_closed` 等的也只是子代**邮箱
//! 关闭**，而那发生在子代 `on_stop` **之前**。全库唯一能等到某个 actor 的 `on_stop` 真正
//! 跑完的只有 `wait_for_shutdown_with_result` —— 而它必须持有那个 actor 的引用。
//!
//! 不等的后果不是理论问题：调用方一返回、tokio runtime 一 drop，正在收尾的子孙会被当场
//! 砍断。而 `on_stop` 里有真活要干 —— `IbkrPublicWsActor` 要把还在等 commission 的成交
//! 补发出去，漏掉会让本地持仓少记一笔，并被下次启动的持仓基线固化下来。
//!
//! # 递归是自动的
//!
//! 父等的是子的 `wait_for_shutdown_with_result`，它完成于子的 `on_stop` **之后**；而子的
//! `on_stop` 第一件事就是等它自己的儿子。于是"等子树"逐层自然嵌套 —— 没有任何地方需要
//! 知道整棵树长什么样，也不需要一张全局登记表。

use kameo::actor::{Actor, ActorId, ActorRef, Spawn};
use kameo::mailbox;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// 子 actor 迟迟不停时，每隔多久告警一次。
///
/// **只告警，不放弃**。中间层一旦"超时即放弃"，超时就会逐层叠加成提前返回：
/// 祖先等 5 秒就走，而它下面那棵子树的合法收尾时间是各层串行之和（IbkrActor 有 5 个
/// 子女，理论上限就是 5 倍），结果根的 wait 提前返回、调用方退出、runtime drop 把仍在
/// 收尾的子孙当场砍断 —— 与这套停机链要修的失效是同一种。
///
/// 唯一的硬 deadline 放在根部（见 `crate::engine::wait_for_shutdown`）：那里对整棵树设
/// 一个总预算，超了如实报错退出，而不是让每一层各自偷偷放弃。
const SLOW_CHILD_WARN_INTERVAL: Duration = Duration::from_secs(5);

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type StopFn = Box<dyn Fn() -> BoxFuture + Send + Sync>;

/// 单个子 actor 的停机句柄。
///
/// 子 actor 在类型擦除的边界后面创建时（如各交易所 actor 经 `ExchangeActorOps` 装配），
/// 父 actor 拿不到具体的 `ActorRef<A>`，就先在能拿到类型的地方造一个句柄带回来，
/// 再 [`ChildGroup::push_handle`] 进组。
pub struct ChildStop {
    id: ActorId,
    name: &'static str,
    stop: StopFn,
}

impl ChildStop {
    pub fn new<A: Actor>(name: &'static str, child: ActorRef<A>) -> Self {
        let id = child.id();
        let stop: StopFn = Box::new(move || {
            let child = child.clone();
            Box::pin(async move {
                // 已经不在了：发信号失败属正常停机竞态，wait 会立即返回
                let _ = child.stop_gracefully().await;
                // 必须是 with_result：`wait_for_shutdown` 只等到邮箱关闭，
                // 而那发生在 `on_stop` 之前，等于没等
                child.wait_for_shutdown_with_result(|_| ()).await;
            })
        });
        Self { id, name, stop }
    }
}

/// 一个 actor 的全部子 actor：**谁 spawn 的谁持有，谁持有谁负责等**。
///
/// 持有强引用有两个作用：停机时等得到它们，以及让它们的 sender 计数永不归零 ——
/// 后者堵死了"没人持有 → 邮箱关闭 → 以 `Normal` 悄悄死掉"这条路，而 `Normal` 会被
/// 判成主动退出、不触发级联。判据的前提因此是结构保证，不是文档约定。
#[derive(Default)]
pub struct ChildGroup {
    children: Vec<ChildStop>,
    /// 是否已经走过 [`Self::shutdown`]，供 `Drop` 守卫判断
    shut_down: bool,
}

impl ChildGroup {
    /// 起一个子 actor：等价于 `spawn_link_with_mailbox`，并把它登记进本组。
    ///
    /// `name` 只用于诊断（停机超时、Drop 守卫），取具体类型名即可。
    ///
    /// 一律 `mailbox::unbounded()`：有界邮箱在这条路径上会**静默丢事件** —— 三条总线都是
    /// `DeliveryStrategy::BestEffort`（`try_tell`），而 `PubSub::publish` 对 `MailboxFull`
    /// 不重试、不报错、也不摘除订阅者。
    pub async fn spawn<A, P>(
        &mut self,
        parent: &ActorRef<P>,
        name: &'static str,
        args: A::Args,
    ) -> ActorRef<A>
    where
        A: Actor + Spawn,
        A::Args: Send,
        P: Actor,
    {
        let child = A::spawn_link_with_mailbox(parent, args, mailbox::unbounded()).await;
        self.push(name, child.clone());
        child
    }

    /// 把一个已存在的子 actor 登记进本组（已经自己 link 过的用它）
    pub fn push<A: Actor>(&mut self, name: &'static str, child: ActorRef<A>) {
        self.children.push(ChildStop::new(name, child));
    }

    /// 登记一个在别处造好的停机句柄（类型擦除边界后面创建的子 actor 用它）
    pub fn push_handle(&mut self, handle: ChildStop) {
        self.children.push(handle);
    }

    /// 摘除一个子 actor（运行期主动撤下时用，如策略实例降级）。
    ///
    /// 不摘的话句柄会一直攒着：等它是无害的（已死的立即返回），但那份强引用会一直留着。
    pub fn remove<A: Actor>(&mut self, child: &ActorRef<A>) {
        self.children.retain(|c| c.id != child.id());
    }

    /// 停掉并等完本组全部子 actor。在 `on_stop` 里调一次。
    ///
    /// 逐个串行：停机顺序是有意义的（登记顺序即停机顺序），并发停会让超时告警交织在
    /// 一起，看不出到底是谁卡住的。
    pub async fn shutdown(&mut self) {
        self.shut_down = true;
        for child in &self.children {
            let mut stop = std::pin::pin!((child.stop)());
            let mut waited = Duration::ZERO;
            loop {
                match tokio::time::timeout(SLOW_CHILD_WARN_INTERVAL, &mut stop).await {
                    Ok(()) => break,
                    Err(_) => {
                        // 继续等 —— 放弃会让超时逐层叠加成提前返回（见常量文档）。
                        // 它慢到什么程度是运维要知道的事，所以持续报，带上是谁。
                        waited += SLOW_CHILD_WARN_INTERVAL;
                        tracing::warn!(
                            child = child.name,
                            waited_s = waited.as_secs(),
                            "子 actor 迟迟未停，继续等待"
                        );
                    }
                }
            }
        }
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        if self.children.is_empty() || self.shut_down {
            return;
        }
        // 两种成因，都是"这些子 actor 的收尾不会被等待"这一个事实：
        // 1. `on_start` 中途失败（订阅出错、依赖起不来）—— 合法路径，kameo 在
        //    `on_start` 返回 Err 时不调 `on_stop`，局部的组就地丢弃。此时子 actor 会
        //    收到父的 Panicked 通知而级联死掉，只是没人等它们收尾。
        // 2. `on_start` 里建了组、spawn 了儿子，却忘了把它塞进 `Self` —— 这是 bug，
        //    而编译器不会报（组确实被用过）。
        // 两者外部表现相同，无法在这里区分，故只陈述事实并列出两种成因。
        let names: Vec<_> = self.children.iter().map(|c| c.name).collect();
        tracing::warn!(
            ?names,
            "ChildGroup 未经 shutdown 就被丢弃，这些子 actor 的收尾不会被等待：\
             要么 on_start 中途失败（此时属正常），要么忘了把 ChildGroup 存进 Self"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::actor::WeakActorRef;
    use kameo::error::{ActorStopReason, Infallible};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<String>>>;

    /// depth > 0 时再起一个儿子，用来验证递归
    struct Node {
        name: &'static str,
        slow: bool,
        log: Log,
        children: ChildGroup,
    }

    impl Actor for Node {
        type Args = (&'static str, u8, Log);
        type Error = Infallible;

        async fn on_start(
            (name, depth, log): Self::Args,
            me: ActorRef<Self>,
        ) -> Result<Self, Self::Error> {
            let mut children = ChildGroup::default();
            if depth > 0 {
                children
                    .spawn::<Node, _>(&me, "child", ("child", depth - 1, log.clone()))
                    .await;
            }
            Ok(Self {
                name,
                // 叶子最慢：模拟 IbkrPublicWsActor 停机补发成交那种网络往返
                slow: depth == 0,
                log,
                children,
            })
        }

        async fn on_stop(
            &mut self,
            _: WeakActorRef<Self>,
            reason: ActorStopReason,
        ) -> Result<(), Self::Error> {
            self.children.shutdown().await;
            if self.slow {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{reason:?}", self.name));
            Ok(())
        }
    }

    /// 只对根调一次 `stop_gracefully`，整棵树必须自下而上收尾完毕 —— 而且是在
    /// 根的 wait 返回**之前**。这是 `wait_for_shutdown` 全部正确性的支点：
    /// 差一点，调用方返回、runtime drop，正在收尾的子孙就被砍断了。
    #[tokio::test]
    async fn stopping_the_root_waits_for_the_whole_tree_bottom_up() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let root = Node::spawn_with_mailbox(("root", 2, log.clone()), mailbox::unbounded());

        root.stop_gracefully().await.expect("发送 Stop");
        root.wait_for_shutdown_with_result(|_| ()).await;

        let order = log.lock().unwrap().clone();
        assert_eq!(
            order,
            vec![
                "child:Normal".to_string(), // 最深的叶子（也最慢）先完成
                "child:Normal".to_string(),
                "root:Normal".to_string(),
            ],
            "必须自下而上收尾，且全程 Normal（正常停机不该产生事故 reason）"
        );
    }

    /// 消费者收到的事件计数（跨用例独立：只有本用例用它）
    static DELIVERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct Consumer;
    struct Event;

    impl Actor for Consumer {
        type Args = ();
        type Error = Infallible;
        async fn on_start(_: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
    }

    impl kameo::message::Message<Event> for Consumer {
        type Reply = ();
        async fn handle(&mut self, _: Event, _: &mut kameo::message::Context<Self, Self::Reply>) {
            DELIVERED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// 生产者在 `on_stop` 里发出的最后一批事件必须**送达**消费者。
    ///
    /// 只验证"两边都停了"是不够的：停机顺序反了的话，消费者先死，生产者补发的事件
    /// 会撞上一个已经关闭的邮箱而丢失 —— 而那正是 `IbkrPublicWsActor` 停机补发等
    /// commission 成交的真实场景（漏掉会让本地持仓少记一笔）。
    #[tokio::test]
    async fn events_emitted_during_producer_shutdown_still_reach_the_consumer() {
        struct Producer(ActorRef<Consumer>);
        impl Actor for Producer {
            type Args = ActorRef<Consumer>;
            type Error = Infallible;
            async fn on_start(c: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
                Ok(Self(c))
            }
            async fn on_stop(
                &mut self,
                _: WeakActorRef<Self>,
                _: ActorStopReason,
            ) -> Result<(), Self::Error> {
                // 停机时补发最后一批
                for _ in 0..3 {
                    let _ = self.0.tell(Event).send().await;
                }
                Ok(())
            }
        }

        let holder = Node::spawn_with_mailbox(("holder", 0, Arc::new(Mutex::new(Vec::new()))), mailbox::unbounded());
        let consumer = Consumer::spawn_with_mailbox((), mailbox::unbounded());

        let mut producers = ChildGroup::default();
        let mut pipeline = ChildGroup::default();
        producers.spawn::<Producer, _>(&holder, "Producer", consumer.clone()).await;
        pipeline.push("Consumer", consumer.clone());

        // 与 ManagerActor::on_stop 同序：先生产端、后管线
        producers.shutdown().await;
        pipeline.shutdown().await;

        assert_eq!(
            DELIVERED.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "生产者 on_stop 补发的事件必须在消费者停机前送达"
        );
        holder.stop_gracefully().await.ok();
    }

    /// 摘除后不再被等待 —— 降级撤下策略实例走这条路
    #[tokio::test]
    async fn removed_child_is_no_longer_waited_for() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let holder = Node::spawn_with_mailbox(("holder", 0, log.clone()), mailbox::unbounded());

        let mut group = ChildGroup::default();
        let child = group
            .spawn::<Node, _>(&holder, "removable", ("removable", 0, log.clone()))
            .await;
        group.remove(&child);
        group.shutdown().await;

        assert!(child.is_alive(), "已摘除的子 actor 不该被 shutdown 带走");
        child.stop_gracefully().await.ok();
        holder.stop_gracefully().await.ok();
    }
}

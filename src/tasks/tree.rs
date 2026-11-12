//! 任务树：组件的生命周期与监督。
//!
//! # 三条规则
//!
//! - **树形**：每个组件是一个 tokio 任务，挂在父节点的某个 [`Group`] 下；根是 [`Scope::root`]。
//!   谁 spawn 的谁负责停、负责等，没有全局登记表。
//! - **出错向上冒**：组件的 `build` / `run` / `stop` 返回 `Err` 或 panic，就记到父节点上并请求
//!   父节点停止；父节点停完后带着这个错误退出，一层层冒到根。返回 `Ok` 是正常退出，只收掉
//!   它自己那一支，父节点照常运行。
//! - **停机自上而下传信号、自下而上收尾**：一个节点的 `run` 返回后，框架按创建顺序逐组停掉
//!   它的子任务（组内并发、组间有序），全部收尾完才调它自己的 [`Task::stop`]。所以祖先的
//!   资源一定活到子孙收尾完 —— 这一点由框架保证，组件不需要记得去等。
//!
//! # 为什么每个节点一个独立的停止信号，而不是父信号派生的子信号
//!
//! 派生信号在父节点被取消的那一刻就全树同时取消，分组的先后（先停生产者、再停消费者）就
//! 没了。这里停止信号只由父节点的框架在轮到这一组时逐个发下去。
//!
//! # 唯一的硬期限在根上
//!
//! 中间层等子任务只告警、不放弃：中间层超时放弃会逐层叠加成提前返回，正在收尾的子孙被
//! runtime drop 砍断。总期限由根的调用方设（见 `crate::engine::bootstrap`）。

use futures_util::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

/// 子任务迟迟不停时，每隔多久告警一次（只告警，不放弃，见模块文档）。
const SLOW_CHILD_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// 一个组件。
///
/// - [`Self::run`] 是它的主循环，**必须在 `scope.stopping()` 完成后尽快返回**（通常是
///   `select!` 里的一个分支，或用 [`crate::tasks::Inbox::recv`]）。框架不会中途丢弃 `run` 的
///   future —— 正在处理的那条消息一定处理完。
/// - [`Self::stop`] 是收尾，在它的全部子任务都收尾完之后才调用。
///
/// 只有一段循环、没有收尾的组件用 [`Group::spawn_fn`]，不必实现本 trait。
pub trait Task: Send + 'static {
    fn run(&mut self, scope: &Scope) -> impl Future<Output = anyhow::Result<()>> + Send;

    fn stop(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        async { Ok(()) }
    }
}

/// 一个节点的上下文：停止信号、子任务分组、向上报错的入口。
///
/// 可以克隆（内部共享）：组件的构造代码和主循环都可以拿着它起子任务。
#[derive(Clone)]
pub struct Scope(Arc<ScopeInner>);

struct ScopeInner {
    name: String,
    /// 本节点的停止请求：父节点框架轮到本组时发出，或本节点自己出错（[`Scope::fail`]）时发出
    stop: CancellationToken,
    /// 按创建顺序排列的子任务分组，停机时依次停
    groups: Mutex<GroupList>,
    /// 子任务冒上来的错误或 [`Scope::fail`] 记下的错误；只保留第一个（根因），其余记日志
    failure: Mutex<Option<anyhow::Error>>,
}

#[derive(Default)]
struct GroupList {
    groups: Vec<Group>,
    /// 已开始停机：此后新建的组一律是关闭的，spawn 会失败
    closed: bool,
}

impl Scope {
    /// 树根。根没有任务在跑它，由调用方驱动：等 [`Self::stopping`]（任何子任务出错都会触发）
    /// 或外部停机信号，再调 [`Self::shutdown`]。
    pub fn root(name: impl Into<String>) -> Self {
        Self::new(name.into())
    }

    fn new(name: String) -> Self {
        Self(Arc::new(ScopeInner {
            name,
            stop: CancellationToken::new(),
            groups: Mutex::new(GroupList::default()),
            failure: Mutex::new(None),
        }))
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// 本节点是否已被请求停止
    pub fn is_stopping(&self) -> bool {
        self.0.stop.is_cancelled()
    }

    /// 本节点被请求停止时完成
    pub fn stopping(&self) -> WaitForCancellationFuture<'_> {
        self.0.stop.cancelled()
    }

    /// 新建一个子任务分组。**停机时按创建顺序逐组停**，组内并发：有先后依赖的子任务
    /// 放进不同的组，按依赖顺序创建。
    pub fn group(&self, name: &'static str) -> Group {
        let mut list = self.0.groups.lock().unwrap();
        let group = Group::new(name, Arc::downgrade(&self.0), list.closed);
        list.groups.push(group.clone());
        group
    }

    /// 记下一个错误并请求本节点停止：本节点停完后带着这个错误退出（冒到父节点）。
    ///
    /// 子任务出错时框架会调它；组件自己遇到"必须整支停下"的情况（如下单结果不确定）
    /// 也调它，见 [`Self::fail_handle`]。
    pub fn fail(&self, err: anyhow::Error) {
        self.0.fail(err);
    }

    /// 一个可以交给别处、事后调用 [`Scope::fail`] 的句柄。只持弱引用：节点已退出时调用无效果。
    pub fn fail_handle(&self) -> FailHandle {
        FailHandle(Arc::downgrade(&self.0))
    }

    /// 停掉本节点的全部子任务（按组创建顺序，组内并发），然后交出记下的错误（若有）。
    ///
    /// 只给根用：非根节点由框架在 `run` 返回后自动走这一步。
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.stop.cancel();
        self.shutdown_groups().await;
        match self.0.failure.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn shutdown_groups(&self) {
        let groups = {
            let mut list = self.0.groups.lock().unwrap();
            list.closed = true;
            std::mem::take(&mut list.groups)
        };
        for group in groups {
            group.shutdown().await;
        }
    }

    fn take_failure(&self) -> Option<anyhow::Error> {
        self.0.failure.lock().unwrap().take()
    }
}

impl ScopeInner {
    fn fail(&self, err: anyhow::Error) {
        {
            let mut slot = self.failure.lock().unwrap();
            match &*slot {
                None => *slot = Some(err),
                Some(first) => tracing::error!(
                    node = %self.name,
                    first = %format!("{first:#}"),
                    error = %format!("{err:#}"),
                    "节点已在因错误停机，又收到一个错误"
                ),
            }
        }
        self.stop.cancel();
    }
}

/// 见 [`Scope::fail_handle`]
#[derive(Clone)]
pub struct FailHandle(Weak<ScopeInner>);

impl FailHandle {
    pub fn fail(&self, err: anyhow::Error) {
        match self.0.upgrade() {
            Some(scope) => scope.fail(err),
            None => tracing::error!(error = %format!("{err:#}"), "节点已退出，错误无处上报"),
        }
    }
}

/// 一个节点下的一组子任务：停机时组内并发停，整组停完才轮到下一组。
#[derive(Clone)]
pub struct Group(Arc<GroupInner>);

struct GroupInner {
    name: &'static str,
    /// 所属节点：子任务出错报到这里。弱引用，避免 节点 → 组 → 节点 的引用环
    owner: Weak<ScopeInner>,
    state: Mutex<GroupState>,
}

struct GroupState {
    closed: bool,
    children: Vec<ChildHandle>,
}

static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(1);

/// spawn 时组已关闭（所属节点已在停机）
#[derive(Debug, thiserror::Error)]
#[error("任务组 {group} 已关闭（所属节点正在停机），不能再起 {child}")]
pub struct GroupClosed {
    pub group: &'static str,
    pub child: String,
}

impl Group {
    fn new(name: &'static str, owner: Weak<ScopeInner>, closed: bool) -> Self {
        Self(Arc::new(GroupInner {
            name,
            owner,
            state: Mutex::new(GroupState {
                closed,
                children: Vec::new(),
            }),
        }))
    }

    /// 起一个子任务，等它构造完成。
    ///
    /// `build` 在子任务自己的 tokio 任务里跑，拿到子任务的 [`Scope`]（可以在里面起孙任务）。
    /// 构造失败时：调用方拿到 `Err`（带根因），**同时**错误照常冒到本组所属节点 ——
    /// 构造失败就是失败，不由调用方选择吞掉。
    ///
    /// 子任务在返回前就已登记进本组：即便调用方不等返回值，停机时也一定会被等到。
    pub async fn spawn<T, B, Fut>(
        &self,
        name: impl Into<String>,
        build: B,
    ) -> anyhow::Result<ChildHandle>
    where
        T: Task,
        B: FnOnce(Scope) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let name = name.into();
        let (started_tx, started_rx) = oneshot::channel();
        let handle = self.start(name.clone(), build, started_tx)?;
        match started_rx.await {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(msg)) => Err(anyhow::anyhow!("{name} 启动失败: {msg}")),
            // 子任务在报告构造结果前就没了：只可能是 runtime 正在关闭
            Err(_) => Err(anyhow::anyhow!("{name} 启动中途被终止")),
        }
    }

    /// 起一个只有一段主循环、没有收尾的子任务。`f` 拿到子任务的 [`Scope`]，返回的 future
    /// 就是它的 `run`：必须在 `scope.stopping()` 后尽快返回。不等待（没有构造阶段）。
    pub fn spawn_fn<F, Fut>(&self, name: impl Into<String>, f: F) -> Result<ChildHandle, GroupClosed>
    where
        F: FnOnce(Scope) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let (started_tx, _) = oneshot::channel();
        self.start(
            name.into(),
            |_scope| async move { Ok(FnTask(Some(f))) },
            started_tx,
        )
    }

    fn start<T, B, Fut>(
        &self,
        name: String,
        build: B,
        started: oneshot::Sender<Result<(), String>>,
    ) -> Result<ChildHandle, GroupClosed>
    where
        T: Task,
        B: FnOnce(Scope) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let scope = Scope::new(name.clone());
        let handle = ChildHandle {
            id: NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed),
            name: Arc::from(name.as_str()),
            stop: scope.0.stop.clone(),
            done: CancellationToken::new(),
        };
        {
            let mut state = self.0.state.lock().unwrap();
            if state.closed {
                return Err(GroupClosed {
                    group: self.0.name,
                    child: name,
                });
            }
            state.children.push(handle.clone());
        }
        let group = Arc::downgrade(&self.0);
        let me = handle.clone();
        tokio::spawn(async move {
            let outcome = run_node(&scope, build, started).await;
            if let Some(group) = group.upgrade() {
                group.state.lock().unwrap().children.retain(|c| c.id != me.id);
                match outcome {
                    Ok(()) => tracing::debug!(task = %me.name, group = group.name, "任务正常退出"),
                    Err(e) => {
                        let e = e.context(format!("{} 出错退出", me.name));
                        tracing::error!(error = %format!("{e:#}"), "任务出错退出，上报父节点");
                        match group.owner.upgrade() {
                            Some(owner) => owner.fail(e),
                            None => tracing::error!(task = %me.name, "父节点已不在，错误无处上报"),
                        }
                    }
                }
            }
            me.done.cancel();
        });
        Ok(handle)
    }

    /// 停掉并等完本组全部子任务（并发），并关闭本组。只由所属节点的框架调用。
    async fn shutdown(&self) {
        let children = {
            let mut state = self.0.state.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.children)
        };
        for child in &children {
            child.stop.cancel();
        }
        futures_util::future::join_all(children.iter().map(|c| c.wait_done(self.0.name))).await;
    }
}

/// 节点的一生：构造 → 运行 → 停子任务 → 收尾。返回值是它最终的退出结果。
async fn run_node<T, B, Fut>(
    scope: &Scope,
    build: B,
    started: oneshot::Sender<Result<(), String>>,
) -> anyhow::Result<()>
where
    T: Task,
    B: FnOnce(Scope) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let mut task = match catch(build(scope.clone())).await {
        Ok(task) => {
            let _ = started.send(Ok(()));
            task
        }
        Err(e) => {
            let _ = started.send(Err(format!("{e:#}")));
            // 构造途中可能已起了孙任务：照样先停它们
            scope.0.stop.cancel();
            scope.shutdown_groups().await;
            return Err(first_error(scope.take_failure(), [Err(e)]));
        }
    };
    let ran = catch(task.run(scope)).await;
    // run 自己返回（正常退出或出错）时也要收掉子树
    scope.0.stop.cancel();
    scope.shutdown_groups().await;
    let stopped = catch(task.stop()).await;
    match (scope.take_failure(), ran, stopped) {
        (None, Ok(()), Ok(())) => Ok(()),
        (failure, ran, stopped) => Err(first_error(failure, [ran, stopped])),
    }
}

/// 取第一个错误作根因，其余记日志（根因优先级：子任务冒上来的 > run > stop）
fn first_error<const N: usize>(
    failure: Option<anyhow::Error>,
    rest: [anyhow::Result<()>; N],
) -> anyhow::Error {
    let mut errors = failure.into_iter().chain(rest.into_iter().filter_map(Result::err));
    let first = errors.next().expect("调用方保证至少有一个错误");
    for other in errors {
        tracing::error!(root_cause = %format!("{first:#}"), error = %format!("{other:#}"), "同一节点的伴随错误");
    }
    first
}

/// panic 转成 Err（带 panic 文本）
async fn catch<R>(fut: impl Future<Output = anyhow::Result<R>>) -> anyhow::Result<R> {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "（非文本 panic）".to_string());
            Err(anyhow::anyhow!("panic: {msg}"))
        }
    }
}

/// [`Group::spawn_fn`] 的适配：一段 future 当 `run`
struct FnTask<F>(Option<F>);

impl<F, Fut> Task for FnTask<F>
where
    F: FnOnce(Scope) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
        let f = self.0.take().expect("run 只调一次");
        f(scope.clone()).await
    }
}

/// 一个子任务的句柄：可以单独停掉它（连同它的子树）并等收尾完。
#[derive(Clone)]
pub struct ChildHandle {
    id: u64,
    name: Arc<str>,
    stop: CancellationToken,
    /// 整个子树收尾完、退出结果已上报后才置位
    done: CancellationToken,
}

impl ChildHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 请求它停止，等它（连同子树）收尾完。它这样退出算正常退出，父节点照常运行。
    pub async fn stop(&self) {
        self.stop.cancel();
        self.wait_done("（单独停止）").await;
    }

    /// 它（连同子树）已收尾完
    pub fn is_finished(&self) -> bool {
        self.done.is_cancelled()
    }

    /// 等它（连同子树）收尾完，不发停止请求
    pub async fn finished(&self) {
        self.done.cancelled().await;
    }

    async fn wait_done(&self, group: &'static str) {
        let mut waited = Duration::ZERO;
        loop {
            match tokio::time::timeout(SLOW_CHILD_WARN_INTERVAL, self.done.cancelled()).await {
                Ok(()) => return,
                Err(_) => {
                    waited += SLOW_CHILD_WARN_INTERVAL;
                    tracing::warn!(task = %self.name, group, waited_s = waited.as_secs(), "子任务迟迟未停，继续等待");
                }
            }
        }
    }
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle").field("name", &self.name).field("id", &self.id).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    type Log = Arc<Mutex<Vec<String>>>;

    fn log() -> Log {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn entries(log: &Log) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    /// 深度为 depth 的链；每层 stop 时记一笔，叶子收尾最慢
    struct Node {
        name: String,
        leaf: bool,
        log: Log,
    }

    impl Task for Node {
        async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
            scope.stopping().await;
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            if self.leaf {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            self.log.lock().unwrap().push(format!("{}:stopped", self.name));
            Ok(())
        }
    }

    fn spawn_chain(
        group: Group,
        depth: u8,
        log: Log,
    ) -> futures_util::future::BoxFuture<'static, anyhow::Result<ChildHandle>> {
        Box::pin(async move {
            let name = format!("d{depth}");
            group
                .spawn(name.clone(), move |scope: Scope| async move {
                    if depth > 0 {
                        spawn_chain(scope.group("kids"), depth - 1, log.clone()).await?;
                    }
                    Ok(Node { name, leaf: depth == 0, log })
                })
                .await
        })
    }

    /// 停根：整棵树自下而上收尾，且根的 shutdown 返回时已全部收尾完
    #[tokio::test]
    async fn shutdown_stops_the_tree_bottom_up() {
        let log = log();
        let root = Scope::root("root");
        spawn_chain(root.group("main"), 2, log.clone()).await.unwrap();

        root.shutdown().await.expect("正常停机不该报错");

        assert_eq!(entries(&log), ["d0:stopped", "d1:stopped", "d2:stopped"]);
    }

    /// 组间有序：先创建的组全部停完，后创建的组才收到停止信号。
    /// 生产者在自己收尾时发的最后一批消息，消费者必须还活着收得到。
    #[tokio::test]
    async fn groups_stop_in_creation_order() {
        let log = log();
        let root = Scope::root("root");
        let producers = root.group("producers");
        let consumers = root.group("consumers");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let consumer_log = log.clone();
        consumers
            .spawn_fn("consumer", move |scope| async move {
                loop {
                    tokio::select! {
                        biased;
                        Some(n) = rx.recv() => consumer_log.lock().unwrap().push(format!("got {n}")),
                        _ = scope.stopping() => break,
                    }
                }
                while let Ok(n) = rx.try_recv() {
                    consumer_log.lock().unwrap().push(format!("got {n}"));
                }
                consumer_log.lock().unwrap().push("consumer stopped".into());
                Ok(())
            })
            .unwrap();

        struct Producer(tokio::sync::mpsc::UnboundedSender<u32>);
        impl Task for Producer {
            async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
                scope.stopping().await;
                Ok(())
            }
            async fn stop(&mut self) -> anyhow::Result<()> {
                tokio::time::sleep(Duration::from_millis(50)).await;
                for n in 0..3 {
                    self.0.send(n)?;
                }
                Ok(())
            }
        }
        producers
            .spawn("producer", move |_| async move { Ok(Producer(tx)) })
            .await
            .unwrap();

        root.shutdown().await.unwrap();
        assert_eq!(entries(&log), ["got 0", "got 1", "got 2", "consumer stopped"]);
    }

    /// 组内并发：本组耗时 ≈ 最慢的那一个，不是全部之和
    #[tokio::test(start_paused = true)]
    async fn children_in_one_group_stop_concurrently() {
        let log = log();
        let root = Scope::root("root");
        let group = root.group("slow");
        for _ in 0..3 {
            spawn_chain(group.clone(), 0, log.clone()).await.unwrap();
        }
        let started = tokio::time::Instant::now();
        root.shutdown().await.unwrap();
        assert_eq!(entries(&log).len(), 3);
        assert!(started.elapsed() < Duration::from_millis(600), "{:?}", started.elapsed());
    }

    /// 叶子出错：错误冒到根（根因保留），整棵树停下且都收了尾
    #[tokio::test]
    async fn child_error_propagates_to_the_root_and_stops_everything() {
        let log = log();
        let root = Scope::root("root");
        let main = root.group("main");
        spawn_chain(main.clone(), 1, log.clone()).await.unwrap();
        let mid_log = log.clone();
        main.spawn("mid", |scope: Scope| async move {
            scope.group("kids").spawn_fn("leaf", |_| async { anyhow::bail!("WS 断了") })?;
            Ok(Node { name: "mid".into(), leaf: false, log: mid_log })
        })
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), root.stopping()).await.expect("出错必须触发根停机");
        let err = root.shutdown().await.expect_err("根必须带着错误退出");
        let text = format!("{err:#}");
        assert!(text.contains("WS 断了") && text.contains("leaf") && text.contains("mid"), "{text}");
        let mut got = entries(&log);
        got.sort();
        assert_eq!(got, ["d0:stopped", "d1:stopped", "mid:stopped"]);
    }

    /// panic 等同出错
    #[tokio::test]
    async fn panic_is_an_error() {
        let root = Scope::root("root");
        root.group("g")
            .spawn_fn("boom", |_| async { panic!("越界") })
            .unwrap();
        root.stopping().await;
        let err = root.shutdown().await.unwrap_err();
        assert!(format!("{err:#}").contains("越界"), "{err:#}");
    }

    /// 正常退出只收掉它自己那一支，父节点照常运行
    #[tokio::test]
    async fn normal_exit_does_not_stop_the_parent() {
        let root = Scope::root("root");
        let h = root.group("g").spawn_fn("done", |_| async { Ok(()) }).unwrap();
        h.finished().await;
        assert!(!root.is_stopping());
        root.shutdown().await.unwrap();
    }

    /// 单独停一个子任务：等到它整支收尾完，不影响父节点
    #[tokio::test]
    async fn stopping_one_child_waits_for_its_subtree() {
        let log = log();
        let root = Scope::root("root");
        let h = spawn_chain(root.group("g"), 1, log.clone()).await.unwrap();
        h.stop().await;
        assert_eq!(entries(&log), ["d0:stopped", "d1:stopped"]);
        assert!(h.is_finished() && !root.is_stopping());
        root.shutdown().await.unwrap();
    }

    /// 构造失败：调用方拿到带根因的 Err，同时冒到父节点；构造中起的孙任务被停掉
    #[tokio::test]
    async fn build_failure_is_reported_to_caller_and_parent() {
        static GRANDCHILD_STOPPED: AtomicUsize = AtomicUsize::new(0);
        let root = Scope::root("root");
        let err = root
            .group("g")
            .spawn("ledger", |scope: Scope| async move {
                scope.group("kids").spawn_fn("poller", |s| async move {
                    s.stopping().await;
                    GRANDCHILD_STOPPED.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })?;
                anyhow::bail!("账本打不开");
                #[allow(unreachable_code)]
                Ok(Node { name: String::new(), leaf: false, log: log() })
            })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("账本打不开"), "{err:#}");
        root.stopping().await;
        assert!(format!("{:#}", root.shutdown().await.unwrap_err()).contains("账本打不开"));
        assert_eq!(GRANDCHILD_STOPPED.load(Ordering::SeqCst), 1);
    }

    /// 停机开始后不能再往这个节点下起任务
    #[tokio::test]
    async fn spawning_into_a_closed_group_fails() {
        let root = Scope::root("root");
        let g = root.group("g");
        root.shutdown().await.unwrap();
        assert!(g.spawn_fn("late", |_| async { Ok(()) }).is_err());
        assert!(root.group("later").spawn_fn("late", |_| async { Ok(()) }).is_err());
    }

    /// fail_handle：组件自己判定必须停机（如下单结果不确定）
    #[tokio::test]
    async fn fail_handle_stops_the_node_with_that_error() {
        let root = Scope::root("root");
        let halt = root.fail_handle();
        halt.fail(anyhow::anyhow!("下单结果不确定"));
        root.stopping().await;
        assert!(format!("{:#}", root.shutdown().await.unwrap_err()).contains("下单结果不确定"));
    }
}

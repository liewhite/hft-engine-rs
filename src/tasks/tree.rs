//! 任务树：组件的生命周期与监督。
//!
//! # 三条规则
//!
//! - **树形**：每个组件是一个 tokio 任务，挂在父节点的某个 [`Group`] 下；根是 [`Root`]。
//!   谁 spawn 的谁负责停、负责等，没有全局登记表。
//! - **出错向上冒**：组件的 `build` / `run` / `stop` 返回 `Err` 或 panic，就记到父节点上并请求
//!   父节点停止，同时立刻请求根停机；父节点停完后带着这个错误退出，一层层冒到根。被要求停止
//!   后返回 `Ok` 是正常退出，只收掉它自己那一支，父节点照常运行。
//! - **停机自上而下传信号、自下而上收尾**：一个节点的 `run` 返回后，框架按声明顺序逐组停掉
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

/// 子任务迟迟不停、或 `run` 收到停止信号后迟迟不返回时，每隔多久告警一次（只告警，不放弃）
const SLOW_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// 一个组件。
///
/// - [`Self::run`] 是它的主循环，**必须在 `scope.stopping()` 完成后尽快返回**（通常是
///   `select!` 里的一个分支，或用 [`crate::tasks::Inbox::recv`]）。框架不会中途丢弃 `run` 的
///   future —— 正在处理的那条消息一定处理完；迟迟不返回会每 5 秒告警。
///   **没被要求停止就返回 `Ok` 算出错**（流结束、循环意外跳出不会被悄悄收掉）；真要收工先调
///   [`Scope::request_stop`]。
/// - [`Self::stop`] 是收尾，在它的全部子任务都收尾完之后才调用；`run` 出错或 panic 也照样调。
///   **收尾时要补发的东西（如成交）直接发到总线，不要经父节点转**：父节点的收件箱在停子任务
///   之前就已关闭，发给它会失败（[`crate::tasks::Gone`]）。反过来也一样：要交给**子任务**的最后一份
///   东西（如停机前的最终快照）在 `run` 结尾发，不要放进 `stop`：那时子任务已停完；而 `run` 返回
///   之前子任务一定还活着，它们停机时会先排空收件箱。
///
/// 只有一段循环、没有收尾的组件用 [`Group::spawn_fn`]，不必实现本 trait。
pub trait Task: Send + 'static {
    fn run(&mut self, scope: &Scope) -> impl Future<Output = anyhow::Result<()>> + Send;

    fn stop(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        async { Ok(()) }
    }
}

/// 一个节点的上下文：停止信号、子任务分组、收件箱、向上报错的入口。
///
/// 可以克隆（内部共享）：组件的构造代码和主循环都可以拿着它起子任务。
#[derive(Clone)]
pub struct Scope(Arc<ScopeInner>);

struct ScopeInner {
    name: String,
    /// 树根的停止信号（根自己就是 `stop` 的克隆）：任何节点出错都立刻请求根停机（见
    /// [`ScopeInner::fail`]）；也是 [`Scope::machine_stop_flag`] 的来源
    root_stop: CancellationToken,
    /// 本节点的停止请求：父节点框架轮到本组时发出，或本节点出错 / 主动收工时发出
    stop: CancellationToken,
    /// 按声明顺序排列的子任务分组，停机时依次停
    groups: Mutex<GroupList>,
    /// 子任务冒上来的错误或 [`Scope::fail`] 记下的错误；只保留第一个（根因），其余记日志
    failure: Mutex<FailureSlot>,
}

#[derive(Default)]
struct GroupList {
    groups: Vec<Group>,
    /// 构造阶段已结束：分组只能在构造阶段声明（停机顺序 = 声明顺序，运行期追加会把新组排到
    /// 最后停，悄悄改变停机语义）
    sealed: bool,
    /// 已开始停机：此后新建的组一律是关闭的，spawn 会失败
    closed: bool,
}

#[derive(Default)]
struct FailureSlot {
    error: Option<anyhow::Error>,
    /// 节点已交出最终结果：此后再上报的错误没人读，只能记日志
    finished: bool,
}

impl Scope {
    /// `root_stop`：所在树根的停止信号；`None` = 本节点就是根
    fn new(name: String, root_stop: Option<CancellationToken>) -> Self {
        let stop = CancellationToken::new();
        Self(Arc::new(ScopeInner {
            name,
            root_stop: root_stop.unwrap_or_else(|| stop.clone()),
            stop,
            groups: Mutex::new(GroupList::default()),
            failure: Mutex::new(FailureSlot::default()),
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

    /// 本节点停止状态的只读视图，交给只需要问「是不是在停机」的协作方（不必把整个
    /// `Scope` —— 连同起子任务、报错、请求停止的能力 —— 交出去）
    pub fn stop_flag(&self) -> StopFlag {
        StopFlag(self.0.stop.clone())
    }

    /// 整机（树根）停止状态的只读视图：任何节点出错、或外部要求停机，根都会**立刻**进入停止，
    /// 早于停止信号按分组顺序逐层传到本节点。给「整机一开始停就不该再做某事」的协作方用
    /// （如实盘下单出口：停机途中不再发新单）。
    pub fn machine_stop_flag(&self) -> StopFlag {
        StopFlag(self.0.root_stop.clone())
    }

    /// 主动收工：请求本节点停止，`run` 随后返回 `Ok` 即算正常退出（见 [`Task::run`]）。
    pub fn request_stop(&self) {
        self.0.stop.cancel();
    }

    /// 新建一个子任务分组。**停机时按声明顺序逐组停**，组内并发：有先后依赖的子任务
    /// 放进不同的组，按依赖顺序声明。
    ///
    /// # Panics
    /// 只能在构造阶段（[`Group::spawn`] 的 `build` 里）调用；运行期再调会 panic（随即按出错
    /// 处理）。运行期新增的子任务往已声明的组里 spawn。
    pub fn group(&self, name: &'static str) -> Group {
        {
            let mut list = self.0.groups.lock().unwrap();
            if !list.sealed {
                let group = Group::new(name, Arc::downgrade(&self.0), list.closed);
                list.groups.push(group.clone());
                return group;
            }
        }
        // 在锁外 panic：持锁 panic 会毒化互斥锁，停机路径随之 panic、子树再也收不了尾
        panic!(
            "{}: 分组 {name} 必须在构造阶段声明（停机顺序 = 声明顺序），运行期只能往已有的组里 spawn",
            self.0.name
        )
    }

    /// 记下一个错误并请求本节点停止：本节点停完后带着这个错误退出（冒到父节点）；同时立刻
    /// 请求根停机。组件自己遇到"必须停下"的情况调它，或经 [`Self::fail_handle`] 交给别处调。
    pub fn fail(&self, err: anyhow::Error) {
        self.0.fail(err);
    }

    /// 一个可以交给别处、事后调用 [`Scope::fail`] 的句柄。只持弱引用。
    pub fn fail_handle(&self) -> FailHandle {
        FailHandle(Arc::downgrade(&self.0))
    }

    /// 新建一个属于本节点的收件箱。收件箱只能从 `Scope` 建：能收消息、能订阅总线的一定是
    /// 挂在树上、有人监督的组件。
    pub fn inbox<M: Send + 'static>(&self) -> (super::Addr<M>, super::Inbox<M>) {
        super::inbox::new(self.clone())
    }

    fn seal(&self) {
        self.0.groups.lock().unwrap().sealed = true;
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

    /// 交出记下的错误，并标记本节点已结束
    fn finish(&self) -> Option<anyhow::Error> {
        let mut slot = self.0.failure.lock().unwrap();
        slot.finished = true;
        slot.error.take()
    }
}

impl ScopeInner {
    fn fail(&self, err: anyhow::Error) {
        // 立刻请求根停机：错误本身仍逐层冒（根因随最终退出结果交出），但"整机要停"不必等
        // 中间每一层的 run 返回 —— 某一层卡在不理会停止信号的 await 上时，根照样开始停机，
        // 卡住的那层由"迟迟未返回/未停"告警和根的总期限暴露出来。
        // 只取消根的信号，不越过分组顺序：停机仍由根自上而下逐组进行。
        // 本节点已结束（错误无处上报）也照样请求：整机停止状态是下单出口的闸门（#710），
        // 不能取决于报错方恰好挂在哪个节点、该节点是否还活着。
        self.root_stop.cancel();
        {
            let mut slot = self.failure.lock().unwrap();
            if slot.finished {
                tracing::error!(node = %self.name, error = %format!("{err:#}"), "节点已结束，错误无处上报");
                return;
            }
            match &slot.error {
                None => slot.error = Some(err),
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

/// 见 [`Scope::stop_flag`]：只能查、不能改
#[derive(Clone, Debug)]
pub struct StopFlag(CancellationToken);

impl StopFlag {
    /// 所属节点是否已被请求停止
    pub fn is_stopping(&self) -> bool {
        self.0.is_cancelled()
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

/// 树根。只有它能 [`Self::shutdown`]；组件拿到的 [`Scope`] 没有这个能力。
///
/// 调用方（`crate::engine::bootstrap`）的用法：声明分组、起子任务，等 [`Self::stopping`]
/// （任何节点出错都会触发）或外部停机信号，再 [`Self::shutdown`]，对它设总期限。
pub struct Root {
    scope: Scope,
    shut_down: bool,
}

impl Root {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            scope: Scope::new(name.into(), None),
            shut_down: false,
        }
    }

    /// 声明一个分组（停机按声明顺序，见 [`Scope::group`]；根上随时可以声明）
    pub fn group(&self, name: &'static str) -> Group {
        self.scope.group(name)
    }

    /// 有节点出错（或 [`Self::fail_handle`] 被调用）时完成
    pub fn stopping(&self) -> WaitForCancellationFuture<'_> {
        self.scope.stopping()
    }

    pub fn is_stopping(&self) -> bool {
        self.scope.is_stopping()
    }

    pub fn fail_handle(&self) -> FailHandle {
        self.scope.fail_handle()
    }

    /// 见 [`Scope::machine_stop_flag`]
    pub fn machine_stop_flag(&self) -> StopFlag {
        self.scope.machine_stop_flag()
    }

    /// 按声明顺序逐组停掉全部子任务，等全部收尾完，交出最终结果（根因错误，若有）。
    /// 返回的 future 被丢弃（如超过总期限）时，未停完的子任务继续收尾直到 runtime 关闭。
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.shut_down = true;
        self.scope.0.stop.cancel();
        self.scope.shutdown_groups().await;
        match self.scope.finish() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        if self.shut_down {
            return;
        }
        // 没走 shutdown 就被丢弃（调用方提前返回或 panic）：子任务的停止信号永远不会再发，
        // 它们会一直跑。尽力而为：先把根自己标为停机（整机停止状态立刻生效，如下单出口拒发新单），
        // 再把信号全部发出去（不等收尾），并大声报出来。
        self.scope.0.stop.cancel();
        let groups = std::mem::take(&mut self.scope.0.groups.lock().unwrap().groups);
        let mut names = Vec::new();
        for g in &groups {
            let mut state = g.0.state.lock().unwrap();
            state.closed = true;
            for c in &state.children {
                c.stop.cancel();
                names.push(c.name.to_string());
            }
        }
        if !names.is_empty() {
            tracing::error!(
                ?names,
                "任务树的根未经 shutdown 就被丢弃：已向子任务发出停止信号，但没人等它们收尾"
            );
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

/// spawn 时组已关闭（所属节点已在停机）。停机途中的正常竞态，调用方不要当事故往上抛。
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
    /// `build` 在子任务自己的 tokio 任务里跑，拿到子任务的 [`Scope`]（在里面声明分组、起孙
    /// 任务、建收件箱）。构造失败时：调用方拿到 `Err`（带根因），**同时**错误照常冒到本组
    /// 所属节点 —— 构造失败就是失败，不由调用方选择吞掉。
    ///
    /// 子任务在返回前就已登记进本组：即便调用方不等返回值，停机时也一定会被等到。
    ///
    /// 本组已关闭时返回的错误可 `downcast_ref::<GroupClosed>()`（见 [`GroupClosed`]）。
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
    /// 就是它的 `run`（规则同 [`Task::run`]）。不等待（没有构造阶段，分组也不能声明）。
    pub fn spawn_fn<F, Fut>(
        &self,
        name: impl Into<String>,
        f: F,
    ) -> Result<ChildHandle, GroupClosed>
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
        let scope = Scope::new(name.clone(), Some(self.root_stop()));
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
            // 框架自身万一 panic（本不该发生）：上报错误再标记完成，父节点既不会挂住、也不会误当正常退出
            let mut guard = ExitGuard {
                done: me.done.clone(),
                owner: group.clone(),
                completed: false,
            };
            let outcome = run_node(&scope, build, started).await;
            let group = group.upgrade();
            // 顺序要紧：先上报，再从组里摘除，最后标记完成。父节点停机时要么还在组里看到它、
            // 等到它完成（此时错误已记下）；要么看不到它（错误更早就记下了）。反过来的话，
            // 父节点可能恰好在摘除之后、上报之前读走"无错误"，一次事故被报成正常停机。
            match outcome {
                Ok(()) => tracing::debug!(task = %me.name, "任务正常退出"),
                Err(e) => {
                    let e = e.context(format!("{} 出错退出", me.name));
                    tracing::error!(error = %format!("{e:#}"), "任务出错退出，上报父节点");
                    match group.as_ref().and_then(|g| g.owner.upgrade()) {
                        Some(owner) => owner.fail(e),
                        None => tracing::error!(task = %me.name, "父节点已不在，错误无处上报"),
                    }
                }
            }
            match &group {
                Some(group) => group
                    .state
                    .lock()
                    .unwrap()
                    .children
                    .retain(|c| c.id != me.id),
                None => {
                    tracing::error!(task = %me.name, "所属任务组已销毁（根未经 shutdown 被丢弃？）")
                }
            }
            guard.completed = true; // guard 在此 drop：最后才标记完成
        });
        Ok(handle)
    }

    /// 本组所在树根的停止信号。所属节点已销毁时（根未经 shutdown 被丢弃）给一个已取消的
    /// 信号：此时整棵树本就在停，新节点不该当作「整机在跑」
    fn root_stop(&self) -> CancellationToken {
        match self.0.owner.upgrade() {
            Some(owner) => owner.root_stop.clone(),
            None => {
                let stopped = CancellationToken::new();
                stopped.cancel();
                stopped
            }
        }
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

/// 子任务包装的退出守卫：无论怎样离开都标记完成；没走到正常结尾（框架自身 panic）时先上报错误
struct ExitGuard {
    done: CancellationToken,
    owner: Weak<GroupInner>,
    completed: bool,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        if !self.completed {
            match self.owner.upgrade().and_then(|g| g.owner.upgrade()) {
                Some(owner) => owner.fail(anyhow::anyhow!("任务框架内部 panic，子任务状态未知")),
                None => tracing::error!("任务框架内部 panic，且父节点已不在"),
            }
        }
        self.done.cancel();
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
    let built = catch(build(scope.clone())).await;
    scope.seal();
    let mut task = match built {
        Ok(task) => {
            let _ = started.send(Ok(()));
            task
        }
        Err(e) => {
            let _ = started.send(Err(format!("{e:#}")));
            // 构造途中可能已起了孙任务：照样先停它们
            scope.0.stop.cancel();
            scope.shutdown_groups().await;
            return Err(first_error(scope.finish(), [Err(e)]));
        }
    };
    let ran = match run_watched(scope, &mut task).await {
        // 没被要求停止就自己返回 Ok：流结束、循环跳出 —— 不能当正常退出悄悄收掉
        Ok(()) if !scope.is_stopping() => Err(anyhow::anyhow!(
            "run 未经停止请求就自行结束（真要收工请先调 Scope::request_stop）"
        )),
        other => other,
    };
    // run 自己返回（正常收工或出错）时也要收掉子树
    scope.0.stop.cancel();
    scope.shutdown_groups().await;
    let stopped = catch(task.stop()).await;
    match (scope.finish(), ran, stopped) {
        (None, Ok(()), Ok(())) => Ok(()),
        (failure, ran, stopped) => Err(first_error(failure, [ran, stopped])),
    }
}

/// 跑 `run`；收到停止信号后它迟迟不返回就每 5 秒告警一次（不理会停止信号的组件由此暴露）
async fn run_watched<T: Task>(scope: &Scope, task: &mut T) -> anyhow::Result<()> {
    let run = catch(task.run(scope));
    tokio::pin!(run);
    tokio::select! {
        r = &mut run => return r,
        _ = scope.stopping() => {}
    }
    let mut waited = Duration::ZERO;
    loop {
        match tokio::time::timeout(SLOW_WARN_INTERVAL, &mut run).await {
            Ok(r) => return r,
            Err(_) => {
                waited += SLOW_WARN_INTERVAL;
                tracing::warn!(task = %scope.name(), waited_s = waited.as_secs(), "已请求停止，run 迟迟未返回");
            }
        }
    }
}

/// 取第一个错误作根因，其余记日志（根因优先级：子任务冒上来的 > run > stop）
fn first_error<const N: usize>(
    failure: Option<anyhow::Error>,
    rest: [anyhow::Result<()>; N],
) -> anyhow::Error {
    let mut errors = failure
        .into_iter()
        .chain(rest.into_iter().filter_map(Result::err));
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
///
/// 不要拿自己的句柄调 [`Self::stop`]（会永远等自己）；要收工用 [`Scope::request_stop`]。
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

    /// 请求它停止，等它（连同子树）收尾完。这样退出算正常退出，父节点照常运行。
    pub async fn stop(&self) {
        self.stop.cancel();
        self.wait_done("（单独停止）").await;
    }

    /// 它（连同子树）已收尾完
    pub fn is_finished(&self) -> bool {
        self.done.is_cancelled()
    }

    async fn wait_done(&self, group: &'static str) {
        let mut waited = Duration::ZERO;
        loop {
            match tokio::time::timeout(SLOW_WARN_INTERVAL, self.done.cancelled()).await {
                Ok(()) => return,
                Err(_) => {
                    waited += SLOW_WARN_INTERVAL;
                    tracing::warn!(task = %self.name, group, waited_s = waited.as_secs(), "子任务迟迟未停，继续等待");
                }
            }
        }
    }
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("name", &self.name)
            .field("id", &self.id)
            .finish()
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

    /// 等停止信号；stop 时记一笔，叶子收尾最慢
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
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:stopped", self.name));
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
                    Ok(Node {
                        name,
                        leaf: depth == 0,
                        log,
                    })
                })
                .await
        })
    }

    /// 停根：整棵树自下而上收尾，且根的 shutdown 返回时已全部收尾完
    #[tokio::test]
    async fn shutdown_stops_the_tree_bottom_up() {
        let log = log();
        let root = Root::new("root");
        spawn_chain(root.group("main"), 2, log.clone())
            .await
            .unwrap();

        root.shutdown().await.expect("正常停机不该报错");

        assert_eq!(entries(&log), ["d0:stopped", "d1:stopped", "d2:stopped"]);
    }

    /// 组间有序：先声明的组全部停完，后声明的组才收到停止信号。
    /// 生产者在自己收尾时发的最后一批消息，消费者必须还活着收得到。
    #[tokio::test]
    async fn groups_stop_in_declaration_order() {
        let log = log();
        let root = Root::new("root");
        let producers = root.group("producers");
        let consumers = root.group("consumers");

        let consumer_log = log.clone();
        let (addr_tx, addr_rx) = oneshot::channel();
        consumers
            .spawn_fn("consumer", move |scope| async move {
                let (addr, mut inbox) = scope.inbox::<u32>();
                addr_tx.send(addr).unwrap();
                while let Some(n) = inbox.recv().await {
                    consumer_log.lock().unwrap().push(format!("got {n}"));
                }
                consumer_log.lock().unwrap().push("consumer stopped".into());
                Ok(())
            })
            .unwrap();
        let addr = addr_rx.await.unwrap();

        struct Producer(crate::tasks::Addr<u32>);
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
            .spawn("producer", move |_| async move { Ok(Producer(addr)) })
            .await
            .unwrap();

        root.shutdown().await.unwrap();
        assert_eq!(
            entries(&log),
            ["got 0", "got 1", "got 2", "consumer stopped"]
        );
    }

    /// 组内并发：本组耗时 ≈ 最慢的那一个，不是全部之和
    #[tokio::test(start_paused = true)]
    async fn children_in_one_group_stop_concurrently() {
        let log = log();
        let root = Root::new("root");
        let group = root.group("slow");
        for _ in 0..3 {
            spawn_chain(group.clone(), 0, log.clone()).await.unwrap();
        }
        let started = tokio::time::Instant::now();
        root.shutdown().await.unwrap();
        assert_eq!(entries(&log).len(), 3);
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "{:?}",
            started.elapsed()
        );
    }

    /// 叶子出错：错误冒到根（根因保留），整棵树停下且都收了尾
    #[tokio::test]
    async fn child_error_propagates_to_the_root_and_stops_everything() {
        let log = log();
        let root = Root::new("root");
        let main = root.group("main");
        spawn_chain(main.clone(), 1, log.clone()).await.unwrap();
        let mid_log = log.clone();
        main.spawn("mid", |scope: Scope| async move {
            scope
                .group("kids")
                .spawn_fn("leaf", |_| async { anyhow::bail!("WS 断了") })?;
            Ok(Node {
                name: "mid".into(),
                leaf: false,
                log: mid_log,
            })
        })
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), root.stopping())
            .await
            .expect("出错必须触发根停机");
        let err = root.shutdown().await.expect_err("根必须带着错误退出");
        let text = format!("{err:#}");
        assert!(
            text.contains("WS 断了") && text.contains("leaf") && text.contains("mid"),
            "{text}"
        );
        let mut got = entries(&log);
        got.sort();
        assert_eq!(got, ["d0:stopped", "d1:stopped", "mid:stopped"]);
    }

    /// 子任务出错与整机停机同时发生：错误不能被当成正常停机吞掉。
    /// 子任务在收到停止信号后才出错（收尾里撤单失败这一类），根照样带着错误退出。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn error_during_shutdown_is_not_lost() {
        for _ in 0..50 {
            let root = Root::new("root");
            root.group("g")
                .spawn_fn("canceller", |scope| async move {
                    scope.stopping().await;
                    tokio::task::yield_now().await;
                    anyhow::bail!("撤单失败")
                })
                .unwrap();
            let err = root.shutdown().await.expect_err("收尾出错必须冒到根");
            assert!(format!("{err:#}").contains("撤单失败"));
        }
    }

    /// 中间层不理会停止信号：叶子出错照样立刻请求根停机（不等中间层的 run 返回）
    #[tokio::test]
    async fn error_reaches_the_root_even_if_a_middle_layer_is_stuck() {
        struct Stuck;
        impl Task for Stuck {
            async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
                std::future::pending().await
            }
        }
        let root = Root::new("root");
        root.group("g")
            .spawn("stuck", |scope: Scope| async move {
                scope
                    .group("kids")
                    .spawn_fn("leaf", |_| async { anyhow::bail!("断流") })?;
                Ok(Stuck)
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), root.stopping())
            .await
            .expect("根必须在中间层卡住时也收到停机请求");
    }

    /// 整机停止视图：别的分支出错的那一刻，深层节点就能看到「整机在停」，
    /// 而它自己的停止信号要等根按分组顺序逐层发下来
    #[tokio::test]
    async fn machine_stop_flag_sees_a_failure_elsewhere_immediately() {
        struct Idle;
        impl Task for Idle {
            async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
                scope.stopping().await;
                Ok(())
            }
        }
        let root = Root::new("root");
        let (flags_tx, flags_rx) = oneshot::channel();
        root.group("a")
            .spawn("mid", |scope: Scope| async move {
                scope.group("kids").spawn_fn("deep", |deep| async move {
                    flags_tx
                        .send((deep.stop_flag(), deep.machine_stop_flag()))
                        .unwrap();
                    deep.stopping().await;
                    Ok(())
                })?;
                Ok(Idle)
            })
            .await
            .unwrap();
        let (own, machine) = flags_rx.await.unwrap();
        assert!(!machine.is_stopping());

        root.group("b")
            .spawn_fn("leaf", |_| async { anyhow::bail!("持仓漂移") })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), root.stopping())
            .await
            .unwrap();
        assert!(machine.is_stopping(), "根一进入停机，整机视图就该是停机");
        assert!(
            !own.is_stopping(),
            "节点自己的停止信号仍按分组顺序由根发下来"
        );
        assert!(root.shutdown().await.is_err());
    }

    /// 根未经 shutdown 就被丢弃：整机停止状态也立刻生效（下单出口靠它拒发新单）
    #[tokio::test]
    async fn dropping_the_root_marks_the_machine_stopping() {
        let root = Root::new("root");
        let machine = root.machine_stop_flag();
        drop(root);
        assert!(machine.is_stopping());
    }

    /// panic 等同出错
    #[tokio::test]
    async fn panic_is_an_error() {
        let root = Root::new("root");
        root.group("g")
            .spawn_fn("boom", |_| async { panic!("越界") })
            .unwrap();
        root.stopping().await;
        let err = root.shutdown().await.unwrap_err();
        assert!(format!("{err:#}").contains("越界"), "{err:#}");
    }

    /// 没被要求停止就自己返回 Ok（流结束、循环跳出）算出错
    #[tokio::test]
    async fn returning_ok_without_a_stop_request_is_an_error() {
        let root = Root::new("root");
        root.group("g")
            .spawn_fn("stream", |_| async { Ok(()) })
            .unwrap();
        root.stopping().await;
        let err = root.shutdown().await.unwrap_err();
        assert!(format!("{err:#}").contains("未经停止请求"), "{err:#}");
    }

    /// 主动收工：只收掉它自己那一支，父节点照常运行
    #[tokio::test]
    async fn requested_exit_does_not_stop_the_parent() {
        let root = Root::new("root");
        let h = root
            .group("g")
            .spawn_fn("done", |scope| async move {
                scope.request_stop();
                Ok(())
            })
            .unwrap();
        h.done.cancelled().await;
        assert!(!root.is_stopping());
        root.shutdown().await.unwrap();
    }

    /// 单独停一个子任务：等到它整支收尾完，不影响父节点
    #[tokio::test]
    async fn stopping_one_child_waits_for_its_subtree() {
        let log = log();
        let root = Root::new("root");
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
        let root = Root::new("root");
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
                Ok(Node {
                    name: String::new(),
                    leaf: false,
                    log: log(),
                })
            })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("账本打不开"), "{err:#}");
        root.stopping().await;
        assert!(format!("{:#}", root.shutdown().await.unwrap_err()).contains("账本打不开"));
        assert_eq!(GRANDCHILD_STOPPED.load(Ordering::SeqCst), 1);
    }

    /// 停机开始后不能再往这个节点下起任务；错误可识别为 GroupClosed
    #[tokio::test]
    async fn spawning_into_a_closed_group_fails() {
        let root = Root::new("root");
        let g = root.group("g");
        root.shutdown().await.unwrap();
        assert!(g.spawn_fn("late", |_| async { Ok(()) }).is_err());
        let err = g
            .spawn("late", |_| async {
                Ok(Node {
                    name: String::new(),
                    leaf: false,
                    log: log(),
                })
            })
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<GroupClosed>().is_some());
    }

    /// 运行期声明分组 = 出错（停机顺序只能在构造阶段定）
    #[tokio::test]
    async fn declaring_a_group_at_runtime_is_an_error() {
        let root = Root::new("root");
        root.group("g")
            .spawn_fn("late-group", |scope| async move {
                let _ = scope.group("too-late");
                Ok(())
            })
            .unwrap();
        root.stopping().await;
        assert!(format!("{:#}", root.shutdown().await.unwrap_err()).contains("构造阶段"));
    }

    /// fail_handle：组件自己判定必须停机（如下单结果不确定）
    #[tokio::test]
    async fn fail_handle_stops_the_root_with_that_error() {
        let root = Root::new("root");
        root.fail_handle().fail(anyhow::anyhow!("下单结果不确定"));
        root.stopping().await;
        assert!(format!("{:#}", root.shutdown().await.unwrap_err()).contains("下单结果不确定"));
    }

    /// 停止状态的只读视图跟着节点走：节点被请求停止后它随之为真
    #[tokio::test]
    async fn stop_flag_follows_the_node() {
        let root = Root::new("root");
        let (flag_tx, flag_rx) = oneshot::channel();
        let h = root
            .group("g")
            .spawn_fn("n", |scope| async move {
                flag_tx.send(scope.stop_flag()).unwrap();
                scope.stopping().await;
                Ok(())
            })
            .unwrap();
        let flag = flag_rx.await.unwrap();
        assert!(!flag.is_stopping());
        h.stop().await;
        assert!(flag.is_stopping());
        root.shutdown().await.unwrap();
    }

    /// 根未经 shutdown 就被丢弃：子任务至少收到停止信号
    #[tokio::test]
    async fn dropping_the_root_signals_children() {
        let root = Root::new("root");
        let h = root
            .group("g")
            .spawn_fn("orphan", |s| async move {
                s.stopping().await;
                Ok(())
            })
            .unwrap();
        drop(root);
        tokio::time::timeout(Duration::from_secs(5), h.done.cancelled())
            .await
            .expect("子任务应收到停止信号");
    }
}

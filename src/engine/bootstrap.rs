use kameo::actor::{Actor, ActorRef, Spawn};
use kameo::mailbox;
use serde::de::DeserializeOwned;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::actor_lifecycle::ChildStop;
use crate::engine::live::{ManagerActor, RegisterSupervisedChild};

/// 初始化 tracing（fmt + EnvFilter + 可选告警外送）
///
/// 默认放行**全部 info**，只把已知吵闹的依赖降到 warn。
///
/// 曾经的默认是 `hft_engine_rs=info`，只覆盖库 —— 各 bin 自身的 `info!`/`warn!` 全部被静默
/// 丢弃，包括"订单只落本地柜台""该 symbol 开始真实下单"这类**操作员必须看到**的提示。
/// 按 target 白名单很容易漏掉新增的 bin，所以改为黑名单式降噪。
///
/// 设置 `ALERT_WEBHOOK_URL` 环境变量即启用告警外送：WARN/ERROR 级事件（对账漂移、
/// 投产失败、断流退出等既有告警点）旁路推送到该 webhook，见
/// [`crate::observability::alert`]。未设置 = 行为与从前完全一致。
pub fn init_tracing() -> anyhow::Result<()> {
    const DEFAULT_FILTER: &str = "info,\
        hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,\
        tungstenite=warn,tokio_tungstenite=warn";
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(DEFAULT_FILTER)
    };
    let alert_config = crate::observability::AlertWebhookConfig::from_env();
    let alert_enabled = alert_config.is_some();
    let alert_layer = alert_config.map(crate::observability::spawn_alert_webhook_layer);
    // EnvFilter 必须作为 fmt 层的 **per-layer filter**，不能挂成全局 layer ——
    // 全局挂载是"所有 layer 的 AND"，RUST_LOG=error 会连带把 WARN 挡在告警层外，
    // 告警外送随控制台冗余度配置静默失效。
    //
    // 告警层同样必须带自己的 per-layer WARN 过滤器：没有过滤器的 layer 会把全局
    // max level hint 退化到 TRACE —— 依赖库每帧的 trace!/debug! 事件全部被构造派发
    // （逐条丢弃），热路径真实性能回退，且只在启用告警的生产环境出现。
    // （不能改在 Layer::enabled 里做级别判断 —— 普通 layer 的 enabled 是全局 AND，
    // 会把 INFO 从控制台也灭掉，正是上面那个 bug 的镜像。）
    tracing_subscriber::registry()
        .with(fmt::layer().with_filter(filter))
        .with(alert_layer.map(|l| l.with_filter(tracing_subscriber::filter::LevelFilter::WARN)))
        .init();
    if alert_enabled {
        tracing::info!("告警外送已启用（ALERT_WEBHOOK_URL），不受 RUST_LOG 控制台过滤影响");
    }
    Ok(())
}

/// 从 CLI 参数读取配置文件并反序列化
pub fn load_config<T: DeserializeOwned>(default_path: &str) -> anyhow::Result<T> {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| default_path.to_string());
    tracing::info!(path = %config_path, "Loading config");
    let content = std::fs::read_to_string(&config_path)?;
    Ok(serde_json::from_str(&content)?)
}

/// 受监督的 ActorRef —— **只能**由 [`spawn_supervised`] 产出。
///
/// 它是一张凭证：持有它即证明该 actor（1）`on_start` 已成功返回，（2）从出生起就
/// link 在监督树上，出错必然级联。订阅接口只收这张凭证，于是"把一个没人看着的
/// actor 订进总线"在**类型上就写不出来** —— 不需要在订阅时再查一遍存活，也就没有
/// "查完到用之间又死了"的窗口。
///
/// 内层 `ActorRef` 私有，外部拿不到构造途径；需要给 actor 发消息时用
/// [`Self::actor_ref`]。
pub struct Supervised<A: Actor>(ActorRef<A>);

impl<A: Actor> Supervised<A> {
    /// 底层 ActorRef，用于发消息
    pub fn actor_ref(&self) -> &ActorRef<A> {
        &self.0
    }

    /// 交出内层引用。crate 内部可见：订阅处理器要把它放进 PubSub。
    pub(crate) fn into_inner(self) -> ActorRef<A> {
        self.0
    }
}

// 手写而非 derive：derive(Clone) 会要求 `A: Clone`，而 actor 类型本身不必可克隆
impl<A: Actor> Clone for Supervised<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<A: Actor> std::fmt::Debug for Supervised<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Supervised").field(&self.0.id()).finish()
    }
}

/// 把一个外部 actor 挂进引擎的监督树。
///
/// 返回 [`Supervised<A>`] —— 一张"已启动成功且已受监督"的凭证，也是订阅接口唯一接受的
/// 入参。返回的 actor **一定已经启动成功、且从出生起就受监督**：此后它因错误死亡
/// （`on_start` 返回 Err、handler panic、被 kill）会经 `ManagerActor::on_link_died`
/// 级联退出整机；而主动 `stop_gracefully()`（`Normal`）只算收工，引擎照常运行。
///
/// # 它堵住的两个洞
///
/// 自己 `spawn` 再 `SubscribeIncome` 的写法有两处静默失败，都不会以任何形式告知调用方：
///
/// - **启动期**：`spawn` 不返回启动结果。`on_start` 里的失败只让 actor 当场死掉
///   （kameo 记为 `Panicked(PanicReason::OnStart)`），日志一行 "actor stopped"，
///   而订阅照发、进程照跑 —— 观察者从第一秒起就是空的。这里 `wait_for_startup_with_result`
///   把它变成调用点的同步 `Err`。
/// - **运行期**：订阅不建立监督关系，订阅者死了只会被 PubSub 静默摘除。这里用
///   `spawn_link_with_mailbox` —— 它**先 link 再 spawn**，actor 从未有过"活着但没人看着"
///   的时刻，之后的死亡必然被观察到。
///
/// # 为什么不用更短的 `spawn_link`
///
/// 两者的 link 时机与监督语义完全一致，唯一区别是邮箱：`spawn_link` 固定给
/// `mailbox::bounded(64)`，而这条路径上有界邮箱会**静默丢事件**。
///
/// 三条总线都是 `DeliveryStrategy::BestEffort`，投递走 `try_tell`（非阻塞，满了立刻失败），
/// 而 `PubSub::publish` 对 `MailboxFull` 的处置是**什么都不做** —— 不重试、不报错、也不摘除
/// 订阅者。观察者只要短暂落后 64 条（写 CSV、发 HTTP 都够），事件就没了，且不留痕迹。
///
/// 无界邮箱的代价是订阅者持续追不上时内存无限增长；但那种情形下有界邮箱只是把"内存涨"
/// 换成"悄悄丢数据"，对账本而言更糟。真要设上限，该配的是显式的丢弃计数与告警。
///
/// 这也是引擎的既有约定：内部所有 actor 一律 `mailbox::unbounded()`，没有例外。
///
/// # 启动失败同样会级联，这是有意的
///
/// `on_start` 返回 `Err` 就是"这确实失败了"，不该由调用方选择吞掉。调用方拿到的 `Err`
/// 在自己的任务里产生，不受 manager 生死影响，所以根因照样报得出来；同时监督树按同一
/// 条判据把整机带下去，不存在"启动失败被当成可选项忽略、进程带着半套观测继续跑"的路径。
///
/// 真正可以容忍的失败，应当由子 actor 在 `on_start` 内部消化掉并正常运行（或自行
/// `stop_gracefully` 退出），而不是把 `Err` 抛出来再指望上层不当回事。
pub async fn spawn_supervised<A>(
    manager: &ActorRef<ManagerActor>,
    args: A::Args,
) -> anyhow::Result<Supervised<A>>
where
    A: Actor + Spawn,
    A::Args: Send,
    A::Error: std::fmt::Display,
{
    let supervised = spawn_supervised_by(manager, args).await?;
    // 登记进 manager 的停机链：只 link 的话出错会级联，但停机时没人等它收尾
    manager
        .tell(RegisterSupervisedChild(ChildStop::new(
            std::any::type_name::<A>(),
            supervised.actor_ref().clone(),
        )))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("登记停机链失败: {e}"))?;
    Ok(supervised)
}

/// [`spawn_supervised`] 的监督者泛型版本。
///
/// 只对 crate 内可见：对外只暴露"挂在 manager 上"这一种，[`Supervised`] 这张凭证才
/// **确切**代表"由引擎监督、出错会整机退出"。放开监督者类型的话，凭证就只剩"被某个
/// 东西监督着"，而那个东西自己可能没人管 —— 证明力就漏了。
/// 泛型版本留给单测：真 `ManagerActor` 的 `on_start` 要联网建 client、拉 symbol
/// metas，起不来。
pub(crate) async fn spawn_supervised_by<A, S>(
    supervisor: &ActorRef<S>,
    args: A::Args,
) -> anyhow::Result<Supervised<A>>
where
    A: Actor + Spawn,
    A::Args: Send,
    A::Error: std::fmt::Display,
    S: Actor,
{
    let actor = A::spawn_link_with_mailbox(supervisor, args, mailbox::unbounded()).await;
    let name = std::any::type_name::<A>();

    actor
        .wait_for_startup_with_result(|result| {
            result.map_err(|e| anyhow::anyhow!("{name} 启动失败: {e}"))
        })
        .await?;

    tracing::info!(actor = name, "actor 已挂入监督树");
    Ok(Supervised(actor))
}

/// 等待停机信号，或 Manager 意外退出。
///
/// 返回 `Ok` 只有一种情形：**收到停机信号并已停机完毕**。Manager 意外终止返回 `Err`，
/// 调用方据此把退出如实报成事故 —— 从前两种归宿都是 `()`，上层只能一律报成
/// "已优雅关闭"，而那在 manager 猝死时是条假消息。
///
/// # 停机只需停根
///
/// 对 manager 一次 `stop_gracefully`，整棵树会自下而上逐层收尾：每个 actor 的 `on_stop`
/// 先等完自己那批子 actor（[`crate::actor_lifecycle::ChildGroup::shutdown`]），自己才停。
/// 全程 `Normal` —— 一次正常停机不该在任何一层留下事故 reason。
///
/// 这一程必须自己走：kameo 的 `link` 建立的是**兄弟**关系，父 actor 停止时子 actor 只会
/// 收到一条通知、不会被带停；而 `wait_for_shutdown` 等到的只是邮箱关闭，那发生在
/// `on_stop` **之前**。两者叠加的后果是：调用方以为等完了，实际正在收尾的子孙被随后的
/// runtime drop 当场砍断。
///
/// # 为什么 SIGTERM 必须和 Ctrl-C 同等对待
///
/// 只接 SIGINT 的话，容器编排的停机路径整条不可达：`docker stop` 与 k8s 滚动重启发的
/// 都是 SIGTERM，进程会被直接杀掉 —— 优雅停机与随之而来的通知在**生产里永远走不到**，
/// 只有开发机按 Ctrl-C 才触发，于是这条路径看着实现了、实际从没运行过。
/// 整棵树的停机总预算。
///
/// 这是**唯一**的硬 deadline：中间层只告警、不放弃（见
/// [`crate::actor_lifecycle`]），否则各层的超时会叠加成提前返回。超了就如实报错，
/// 不再假装停干净了 —— 容器编排随后会用 SIGKILL 收尾。
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

pub async fn wait_for_shutdown(manager: ActorRef<ManagerActor>) -> anyhow::Result<()> {
    tracing::info!("System running. Press Ctrl+C (or send SIGTERM) to stop.");

    let outcome = tokio::select! {
        // 按声明顺序判定，不随机：manager 已经死了之后信号才到时（进程正在因事故退出，
        // 编排随即发来 SIGTERM），两个分支会同时就绪。随机选有约一半概率把一次事故
        // 报成"收到停止信号，已优雅关闭" —— 那是条假消息。事故分支放在前面。
        biased;

        // 带上停止原因：manager 因监督树上某个 actor 出错而级联退出时，reason 是
        // `LinkDied { id, reason }`，死者与死因都在里面。只报"manager 死了"会把排查
        // 起点丢掉 —— 而这条错误往往是运维手里唯一的线索。
        reason = manager.wait_for_shutdown_with_result(|r| match r {
            Ok(reason) => format!("{reason:?}"),
            Err(e) => e.to_string(),
        }) => {
            Err(anyhow::anyhow!("ManagerActor 意外终止，引擎已停止交易: {reason}"))
        }

        signal = shutdown_signal() => {
            tracing::info!(signal, "Received shutdown signal");
            // 只需停根：manager 的 `on_stop` 会等自己那批子 actor，而它们的 `on_stop`
            // 又各自在等下一层 —— 递归自动成立（见 `crate::actor_lifecycle`）。
            if let Err(e) = manager.stop_gracefully().await {
                tracing::warn!(error = %e, "Failed to stop manager gracefully");
            }
            // 必须是 with_result：`wait_for_shutdown` 只等到邮箱关闭，那发生在
            // `on_stop` 之前 —— 用它等于没等，整棵树的收尾会被随后的 runtime drop 砍断。
            let stopped = manager.wait_for_shutdown_with_result(|_| ());
            tokio::select! {
                biased;

                r = tokio::time::timeout(SHUTDOWN_DEADLINE, stopped) => match r {
                    Ok(()) => Ok(()),
                    Err(_) => Err(anyhow::anyhow!(
                        "停机超时（{}s）：仍有 actor 未收尾完成，日志里的\"迟迟未停\"告警指出了是谁",
                        SHUTDOWN_DEADLINE.as_secs()
                    )),
                },
                // 停机卡住时，操作员再发一次信号应当立刻退出 —— 否则只能上 SIGKILL
                second = shutdown_signal() => {
                    Err(anyhow::anyhow!("停机中收到第二次 {second}，放弃等待收尾直接退出"))
                }
            }
        }
    };

    tracing::info!("System stopped");
    outcome
}

/// 停机信号：SIGINT（Ctrl-C）与 SIGTERM（容器编排）等价，返回先到的那个的名字。
///
/// 非 unix 平台上只有 Ctrl-C —— 那里没有 SIGTERM 这个概念。
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // 装不上信号处理器属于进程级异常（信号被占用 / 资源耗尽）。不能静默降级成
        // "只收 Ctrl-C"—— 那会让停机路径悄悄退回修复之前的样子。
        let mut sigterm = signal(SignalKind::terminate()).expect("安装 SIGTERM 处理器失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

#[cfg(test)]
mod supervision_tests {
    use super::*;
    use kameo::error::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// 假监督者：只用来收 link 死亡通知，行为与 ManagerActor::on_link_died 一致
    struct Supervisor;

    impl Actor for Supervisor {
        type Args = ();
        type Error = Infallible;
        async fn on_start(_: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
        async fn on_link_died(
            &mut self,
            _: kameo::actor::WeakActorRef<Self>,
            id: kameo::actor::ActorId,
            reason: kameo::error::ActorStopReason,
        ) -> Result<std::ops::ControlFlow<kameo::error::ActorStopReason>, Self::Error> {
            if reason.is_normal() {
                return Ok(std::ops::ControlFlow::Continue(()));
            }
            Ok(std::ops::ControlFlow::Break(
                kameo::error::ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                },
            ))
        }
    }

    static TICKS: AtomicUsize = AtomicUsize::new(0);
    static STOPS: AtomicUsize = AtomicUsize::new(0);

    /// 记录"处理了多少消息"与"on_stop 跑了几次"，用来验证排空与级联
    struct Recorder;

    impl Actor for Recorder {
        type Args = ();
        type Error = Infallible;
        async fn on_start(_: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
        async fn on_stop(
            &mut self,
            _: kameo::actor::WeakActorRef<Self>,
            _: kameo::error::ActorStopReason,
        ) -> Result<(), Self::Error> {
            STOPS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Tick;

    impl kameo::message::Message<Tick> for Recorder {
        type Reply = ();
        async fn handle(&mut self, _: Tick, _: &mut kameo::message::Context<Self, Self::Reply>) {
            TICKS.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `fail` 为真时 on_start 返回 Err
    struct Observer;

    impl Actor for Observer {
        type Args = bool;
        type Error = anyhow::Error;
        async fn on_start(fail: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            anyhow::ensure!(!fail, "账本打不开");
            Ok(Self)
        }
    }

    /// 启动失败必须是调用点的 `Err`（带根因），**并且**级联退出监督者。
    ///
    /// 两件事缺一不可：只报 Err 而不级联，等于把"这确实失败了"降级成调用方可以
    /// 忽略的建议；只级联而不报 Err，根因就丢了。
    #[tokio::test]
    async fn startup_failure_surfaces_as_error_and_cascades() {
        let supervisor = Supervisor::spawn_with_mailbox((), mailbox::unbounded());
        let err = spawn_supervised_by::<Observer, _>(&supervisor, true)
            .await
            .expect_err("on_start 失败必须传播");
        let text = err.to_string();
        assert!(text.contains("启动失败"), "错误要点名启动阶段: {text}");
        assert!(text.contains("账本打不开"), "错误要带上根因: {text}");

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !supervisor.is_alive(),
            "启动失败被当成可选项放过了 —— 进程会带着半套观测继续跑"
        );
    }

    /// 起来之后出错死亡 → 监督者级联退出
    #[tokio::test]
    async fn error_death_after_startup_cascades() {
        let supervisor = Supervisor::spawn_with_mailbox((), mailbox::unbounded());
        let observer = spawn_supervised_by::<Observer, _>(&supervisor, false)
            .await
            .expect("启动成功");

        observer.actor_ref().kill();
        observer.actor_ref().wait_for_shutdown().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!supervisor.is_alive(), "受监督 actor 出错死亡却没有级联退出");
    }

    /// 停机是靠 kill 级联下去的，而且**先排空邮箱再停** —— `wait_for_shutdown` 的
    /// 全部正确性都压在这条上：
    ///
    /// - 若不级联，子孙会一直活到 runtime 被 drop，`on_stop` 里的收尾（写最后一份
    ///   快照、补发挂起的成交）全部丢失；
    /// - 若不排空，停机瞬间在途的事件就没了。
    ///
    /// `Signal::LinkDied` 与 `Signal::Message` 共用同一条 mpsc 队列，所以两者兼得；
    /// 这个前提哪天变了，本用例会先炸。
    #[tokio::test]
    async fn killing_the_root_cascades_and_drains_first() {
        let root = Supervisor::spawn_with_mailbox((), mailbox::unbounded());
        let child = Recorder::spawn_link_with_mailbox(&root, (), mailbox::unbounded()).await;
        let grandchild = Recorder::spawn_link_with_mailbox(&child, (), mailbox::unbounded()).await;

        // 先把消息排进子孙的邮箱，再 kill 根
        for _ in 0..3 {
            child.tell(Tick).send().await.expect("入队");
            grandchild.tell(Tick).send().await.expect("入队");
        }
        root.kill();
        root.wait_for_shutdown().await;
        child.wait_for_shutdown().await;
        grandchild.wait_for_shutdown().await;

        assert!(!child.is_alive() && !grandchild.is_alive(), "整棵树都该停了");
        assert_eq!(
            (TICKS.load(Ordering::SeqCst), STOPS.load(Ordering::SeqCst)),
            (6, 2),
            "排队的 6 条必须先处理完，两个 on_stop 都要跑到"
        );
    }

    /// 主动退出（stop_gracefully → Normal）→ 监督者继续运行，不需要事先登记意图
    #[tokio::test]
    async fn graceful_stop_does_not_cascade() {
        let supervisor = Supervisor::spawn_with_mailbox((), mailbox::unbounded());
        let observer = spawn_supervised_by::<Observer, _>(&supervisor, false)
            .await
            .expect("启动成功");

        observer.actor_ref().stop_gracefully().await.expect("发送 Stop");
        observer.actor_ref().wait_for_shutdown().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(supervisor.is_alive(), "主动收工却把监督者带走了");
    }
}

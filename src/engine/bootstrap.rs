use serde::de::DeserializeOwned;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::engine::live::{Manager, ManagerArgs, ManagerHandle};
use crate::tasks::Root;

/// 退出前等告警发完的最长时间（单次发送超时 10s，留出一条告警加一条汇总的余量）
const ALERT_FLUSH_DEADLINE: Duration = Duration::from_secs(20);

/// 进程入口：各启动程序的 `main` 只写 `run_main(run).await`。
///
/// 1. 初始化日志与告警外送；配了告警就先试发一条，不通直接拒绝启动 —— 告警是停机类事故
///    唯一的及时通知渠道，配了却不通等于没配，而运行者以为配了。
/// 2. 装 panic hook：任何线程、任何任务 panic 都先记一条 ERROR（于是也会外送）。
/// 3. 跑 `body`；它出错或 panic 都转成退出原因，不占名额直接外送。
/// 4. 在 [`ALERT_FLUSH_DEADLINE`] 内等告警发完再返回 —— 否则进程退出会丢掉最要紧的那几条。
pub async fn run_main<F, Fut>(body: F) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use futures_util::FutureExt;

    let alerts = init_tracing()?;
    // 第一行业务日志就是版本：日志里看本次启动跑的是哪版代码（体检脚本按它报版本）
    tracing::info!(build = crate::build_info::BUILD_ID, "程序启动");
    if let Some(a) = &alerts {
        if let Err(e) = a.probe().await {
            tracing::error!(error = %e, "告警外送不通，拒绝启动");
            return Err(
                e.context("告警外送不通，拒绝启动（修好告警配置，或去掉告警环境变量明确关闭）")
            );
        }
        tracing::info!("告警外送已启用并试发成功，不受 RUST_LOG 控制台过滤影响");
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "panic");
        default_hook(info);
    }));

    let result = match std::panic::AssertUnwindSafe(body()).catch_unwind().await {
        Ok(r) => r,
        Err(payload) => Err(anyhow::anyhow!(
            "主流程 panic: {}",
            crate::tasks::tree::panic_text(&*payload)
        )),
    };
    if let Err(e) = &result {
        let reason = format!("{e:#}");
        match &alerts {
            Some(a) => a.fatal(&reason),
            None => tracing::error!(reason, "进程因错误退出"),
        }
    }
    if let Some(a) = alerts {
        if !a.flush(ALERT_FLUSH_DEADLINE).await {
            tracing::error!(
                "退出前告警未在 {}s 内发完，可能有告警未送达",
                ALERT_FLUSH_DEADLINE.as_secs()
            );
        }
    }
    result
}

/// 初始化 tracing（fmt + EnvFilter + 可选告警外送），返回告警句柄（未配置为 None）。
/// 只由 [`run_main`] 调用：启动试发与退出前发完都在那里。
///
/// 默认放行**全部 info**，只把已知吵闹的依赖降到 warn。
///
/// 曾经的默认是 `hft_engine_rs=info`，只覆盖库 —— 各 bin 自身的 `info!`/`warn!` 全部被静默
/// 丢弃，包括"订单只落本地柜台""该 symbol 开始真实下单"这类**操作员必须看到**的提示。
/// 按 target 白名单很容易漏掉新增的 bin，所以改为黑名单式降噪。
///
/// 设置 `SLACK_BOT_TOKEN`+`SLACK_ALERT_CHANNEL` 或 `ALERT_WEBHOOK_URL` 环境变量即启用告警外送：WARN/ERROR 级事件（对账漂移、
/// 投产失败、断流退出等既有告警点）旁路外送，见
/// [`crate::observability::alert`]。未设置 = 行为与从前完全一致。
fn init_tracing() -> anyhow::Result<Option<crate::observability::AlertHandle>> {
    const DEFAULT_FILTER: &str = "info,\
        hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,\
        tungstenite=warn,tokio_tungstenite=warn";
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(DEFAULT_FILTER)
    };
    let (alert_layer, alert_handle) = match crate::observability::AlertWebhookConfig::from_env()? {
        Some(config) => {
            let (layer, handle) = crate::observability::spawn_alert_webhook_layer(config)?;
            (Some(layer), Some(handle))
        }
        None => (None, None),
    };
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
    Ok(alert_handle)
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

/// 实盘引擎的完整一生：启动 → 装配 → 运行 → 停机。各启动程序只写装配那一段。
///
/// 任务树：根 `engine` 持有唯一的停机总期限，`main` 组下只有 [`Manager`] 一个节点
/// （它下面是生产者、消费者两组，见 `engine::live::manager`）。
///
/// 1. 起 Manager 并等它构造完成（此时各所私有推送都已确认订阅生效，见
///    `crate::exchange::ws_loop::WsConn::await_subscriptions`）；启动失败 → 收尾后返回 `Err`。
/// 2. 跑 `setup`（查品种、加策略、挂观察者、起旁路任务……），拿到 [`ManagerHandle`]。
///    `setup` 出错 → 走同一条停机路径（[`shutdown`]）后返回该错误：装配到一半的引擎
///    （可能已加了部分策略、已在下单）不会被丢下不管。
/// 3. 等停机信号或任何节点出错（[`run_until_shutdown`]）。
///
/// 返回 `Ok` 只有一种情形：**收到停机信号并已停机完毕**。
pub async fn run_engine<F, Fut>(args: ManagerArgs, setup: F) -> anyhow::Result<()>
where
    F: FnOnce(ManagerHandle) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let root = Root::new("engine");
    let main = root.group("main");
    let manager = match Manager::spawn(&main, args).await {
        Ok(manager) => manager,
        Err(e) => {
            // 构造途中可能已起了部分子任务（框架已先收掉 manager 那一支），照样走一遍停机：
            // 根交出的就是同一个根因，这里只记停机本身有没有完成
            if let Err(stop_err) = shutdown(root).await {
                tracing::warn!(error = %format!("{stop_err:#}"), "Manager 启动失败后的停机结果");
            }
            return Err(e.context("Manager 启动失败"));
        }
    };
    drive(root, manager, setup).await
}

/// [`run_engine`] 启动之后的部分，对句柄类型泛化以便单测（真 Manager 要联网，起不来）
async fn drive<H, F, Fut>(root: Root, handle: H, setup: F) -> anyhow::Result<()>
where
    F: FnOnce(H) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    if let Err(e) = setup(handle).await {
        tracing::error!(error = %format!("{e:#}"), "引擎装配失败，停机");
        if let Err(stop_err) = shutdown(root).await {
            tracing::error!(error = %format!("{stop_err:#}"), "装配失败后的停机未完成或伴随错误");
        }
        return Err(e.context("引擎装配失败（已停机）"));
    }
    run_until_shutdown(root).await
}

/// 等待停机信号，或任何节点出错（任务树把错误冒到根，[`Root::stopping`] 随即完成）。
///
/// 节点出错返回 `Err`（带根因），调用方据此把退出如实报成事故 —— 只报"已优雅关闭"
/// 在出事时是条假消息。
///
/// # 为什么 SIGTERM 必须和 Ctrl-C 同等对待
///
/// 只接 SIGINT 的话，容器编排的停机路径整条不可达：`docker stop` 与 k8s 滚动重启发的
/// 都是 SIGTERM，进程会被直接杀掉 —— 优雅停机与随之而来的通知在**生产里永远走不到**，
/// 只有开发机按 Ctrl-C 才触发，于是这条路径看着实现了、实际从没运行过。
async fn run_until_shutdown(root: Root) -> anyhow::Result<()> {
    tracing::info!("System running. Press Ctrl+C (or send SIGTERM) to stop.");

    let signal = tokio::select! {
        // 按声明顺序判定，不随机：节点已经出错之后信号才到时（进程正在因事故退出，
        // 编排随即发来 SIGTERM），两个分支会同时就绪。随机选有约一半概率把一次事故
        // 报成"收到停止信号，已优雅关闭" —— 那是条假消息。事故分支放在前面。
        biased;

        _ = root.stopping() => None,
        signal = shutdown_signal() => Some(signal),
    };

    let outcome = match signal {
        // 根因随停机结果交出（第一个冒上来的错误）：死者与死因都在里面，只报"引擎停了"
        // 会把排查起点丢掉 —— 而这条错误往往是运维手里唯一的线索。
        None => match shutdown(root).await {
            Err(e) => Err(e.context("引擎因节点出错停止交易")),
            Ok(()) => Err(anyhow::anyhow!(
                "引擎被内部请求停机，但没有记下错误（不该发生）"
            )),
        },
        Some(signal) => {
            tracing::info!(signal, "Received shutdown signal");
            shutdown(root).await
        }
    };

    tracing::info!("System stopped");
    outcome
}

/// 整棵树的停机总预算。
///
/// 这是**唯一**的硬期限：中间层只告警、不放弃（见 `crate::tasks::tree` 模块文档），否则
/// 各层的超时会叠加成提前返回。超了就如实报错，不再假装停干净了 —— 容器编排随后会用
/// SIGKILL 收尾。
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

/// 唯一的停机路径（收到信号、节点出错、装配失败、启动失败都走这里）。
///
/// 停根即可：任务树自上而下逐组发停止信号（manager 下先生产者、后消费者），自下而上收尾，
/// 每个节点的子任务全部收尾完才轮到它自己。返回根因错误（若有）。
async fn shutdown(root: Root) -> anyhow::Result<()> {
    tokio::select! {
        biased;

        r = tokio::time::timeout(SHUTDOWN_DEADLINE, root.shutdown()) => match r {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "停机超时（{}s）：仍有任务未收尾完成，日志里的\"迟迟未停\"告警指出了是谁",
                SHUTDOWN_DEADLINE.as_secs()
            )),
        },
        // 停机卡住时，操作员再发一次信号应当立刻退出 —— 否则只能上 SIGKILL
        second = shutdown_signal() => {
            Err(anyhow::anyhow!("停机中收到 {second}，放弃等待收尾直接退出"))
        }
    }
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
    use crate::tasks::{Addr, Bus, ChildHandle, Group, Inbox, Scope, Task};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// 什么都不做、等停止信号的观察者
    struct Idle;

    impl Task for Idle {
        async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
            scope.stopping().await;
            Ok(())
        }
    }

    /// `fail` 为真时构造失败（模拟观察者打不开自己的资源）
    async fn spawn_observer(group: &Group, fail: bool) -> anyhow::Result<ChildHandle> {
        group
            .spawn("Observer", move |_scope| async move {
                anyhow::ensure!(!fail, "账本打不开");
                Ok(Idle)
            })
            .await
    }

    #[derive(Default)]
    struct Counters {
        ticks: AtomicUsize,
        stops: AtomicUsize,
    }

    /// 记录"处理了多少消息"与"收尾跑了几次"，用来验证排空与级联
    struct Recorder {
        inbox: Inbox<()>,
        counters: Arc<Counters>,
    }

    impl Task for Recorder {
        async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
            while self.inbox.recv().await.is_some() {
                self.counters.ticks.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            self.counters.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// 起一个 Recorder；`with_child` 为真时它在构造时再起一个 Recorder 子任务。
    /// 返回两者的地址（没有子任务时第二个为 None）。
    async fn spawn_recorder(
        group: &Group,
        counters: Arc<Counters>,
        with_child: bool,
    ) -> (Addr<()>, Option<Addr<()>>) {
        let (child_tx, child_rx) = tokio::sync::oneshot::channel();
        let c = counters.clone();
        let (_, addr) = group
            .spawn_addressed("Recorder", move |scope: Scope, inbox| async move {
                if with_child {
                    let grand = scope.group("children");
                    let (_, grand_addr) = grand
                        .spawn_addressed("GrandRecorder", {
                            let c = c.clone();
                            move |_scope: Scope, inbox| async move {
                                Ok(Recorder { inbox, counters: c })
                            }
                        })
                        .await?;
                    let _ = child_tx.send(grand_addr);
                }
                Ok(Recorder { inbox, counters: c })
            })
            .await
            .expect("Recorder 起不来");
        (addr, child_rx.await.ok())
    }

    /// 启动失败必须是调用点的 `Err`（带根因），**并且**级联到根、整机停机。
    ///
    /// 两件事缺一不可：只报 Err 而不级联，等于把"这确实失败了"降级成调用方可以
    /// 忽略的建议；只级联而不报 Err，根因就丢了。
    #[tokio::test]
    async fn startup_failure_surfaces_as_error_and_cascades() {
        let root = Root::new("engine");
        let main = root.group("main");
        let err = spawn_observer(&main, true)
            .await
            .expect_err("构造失败必须传播");
        let text = format!("{err:#}");
        assert!(text.contains("启动失败"), "错误要点名启动阶段: {text}");
        assert!(text.contains("账本打不开"), "错误要带上根因: {text}");
        assert!(
            root.is_stopping(),
            "启动失败被当成可选项放过了 —— 进程会带着半套观测继续跑"
        );
        let root_err = root.shutdown().await.expect_err("根要交出根因");
        assert!(format!("{root_err:#}").contains("账本打不开"));
    }

    /// 起来之后出错 → 整机停机，`run_engine` 的等待路径把它报成事故（带根因），
    /// 而不是"收到信号、已优雅关闭"
    #[tokio::test]
    async fn error_death_after_startup_cascades() {
        let root = Root::new("engine");
        let main = root.group("main");
        main.spawn_fn("Observer", |_scope| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            anyhow::bail!("轮询器崩了")
        })
        .unwrap();
        let err = drive(root, (), |_| async { Ok(()) })
            .await
            .expect_err("节点出错必须报成事故");
        let text = format!("{err:#}");
        assert!(text.contains("轮询器崩了"), "要带根因: {text}");
    }

    /// 停机自上而下级联到孙任务，而且**先排空收件箱再停** —— 停机路径（[`shutdown`]）的
    /// 全部正确性都压在这条上：
    ///
    /// - 若不级联，子孙会一直活到 runtime 被 drop，收尾（写最后一份快照、补发挂起的成交）
    ///   全部丢失；
    /// - 若不排空，停机瞬间在途的事件就没了。
    #[tokio::test]
    async fn shutdown_cascades_and_drains_first() {
        let root = Root::new("engine");
        let counters = Arc::new(Counters::default());
        let (child, grandchild) = spawn_recorder(&root.group("main"), counters.clone(), true).await;
        let grandchild = grandchild.expect("孙任务地址");

        // 先把消息排进子孙的收件箱，再停根
        for _ in 0..3 {
            child.send(()).expect("入队");
            grandchild.send(()).expect("入队");
        }
        shutdown(root).await.expect("正常停机");

        assert!(child.is_gone() && grandchild.is_gone(), "整棵树都该停了");
        assert_eq!(
            (
                counters.ticks.load(Ordering::SeqCst),
                counters.stops.load(Ordering::SeqCst)
            ),
            (6, 2),
            "排队的 6 条必须先处理完，两个收尾都要跑到"
        );
    }

    /// 装配失败走同一条停机路径：整棵树已收尾，再返回带根因的错误
    #[tokio::test]
    async fn setup_failure_stops_the_engine_before_returning() {
        let root = Root::new("engine");
        let counters = Arc::new(Counters::default());
        let (addr, _) = spawn_recorder(&root.group("main"), counters.clone(), false).await;
        let err = drive(root, (), |_| async { anyhow::bail!("加策略失败") })
            .await
            .expect_err("装配失败必须返回 Err");
        assert!(
            format!("{err:#}").contains("加策略失败"),
            "要带根因: {err:#}"
        );
        assert!(addr.is_gone(), "装配失败后引擎没停 —— 半套引擎被丢下继续跑");
        assert_eq!(counters.stops.load(Ordering::SeqCst), 1, "收尾要跑到");
    }

    /// 主动收工（自己的句柄 stop）→ 只收掉它自己，引擎继续运行，不需要事先登记意图
    #[tokio::test]
    async fn graceful_stop_does_not_cascade() {
        let root = Root::new("engine");
        let observer = spawn_observer(&root.group("main"), false)
            .await
            .expect("启动成功");
        observer.stop().await;
        assert!(observer.is_finished());
        assert!(!root.is_stopping(), "主动收工却把整机带走了");
        root.shutdown().await.expect("正常停机不该有错误");
    }

    /// manager 的两组停机顺序：生产者收尾时补发的事件，后停的消费者照样收得到
    /// （IBKR 收尾补发等 commission 的成交就靠这条）
    #[tokio::test]
    async fn events_published_by_producers_on_stop_reach_consumers() {
        struct FlushOnStop(Bus<u32>);
        impl Task for FlushOnStop {
            async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
                scope.stopping().await;
                Ok(())
            }
            async fn stop(&mut self) -> anyhow::Result<()> {
                self.0.publish(42);
                Ok(())
            }
        }
        struct Sink(Inbox<u32>, Arc<Mutex<Vec<u32>>>);
        impl Task for Sink {
            async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
                while let Some(n) = self.0.recv().await {
                    self.1.lock().unwrap().push(n);
                }
                Ok(())
            }
        }

        let bus = Bus::<u32>::new("t");
        let got = Arc::new(Mutex::new(Vec::new()));
        let root = Root::new("engine");
        let producers = root.group("producers");
        let consumers = root.group("consumers");
        let (b, g) = (bus.clone(), got.clone());
        consumers
            .spawn("Sink", move |scope| async move {
                let (_addr, mut inbox) = scope.inbox::<u32>();
                inbox.subscribe(&b, |_| true, |n| n);
                Ok(Sink(inbox, g))
            })
            .await
            .unwrap();
        let b = bus.clone();
        producers
            .spawn("Producer", move |_scope| async move { Ok(FlushOnStop(b)) })
            .await
            .unwrap();
        shutdown(root).await.expect("正常停机");
        assert_eq!(*got.lock().unwrap(), [42], "生产者收尾补发的事件丢了");
    }
}

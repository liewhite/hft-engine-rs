//! 告警外送：tracing Layer 旁路 + 后台发送任务。
//!
//! # 为什么挂在 tracing 上，而不是新建一条告警事件通道
//!
//! 引擎的全部告警点（对账漂移、投产失败、WS 断流 kill……）都已经是 WARN/ERROR 级的
//! 结构化日志 —— 告警的本质就是"这些日志必须有人立刻看到"。挂 Layer 零侵入地覆盖
//! 全部既有与将来的告警点；若另建通道，每个告警点都要记得多发一份，漏一处就是盲区。
//!
//! # 一条告警的去向（全在发送任务里，见 [`Throttle`]）
//!
//! - 同一处日志（同一调用点）在一个窗口内只外送第一条，其余计数，窗口结束时汇总一条。
//!   注意按调用点而不是按内容：同一处 ERROR 先报 BTC 再报 ETH，ETH 只出现在窗口末的汇总里；
//! - ERROR 与 WARN 各有自己的每窗口名额 —— 吵闹的 WARN 挤不掉对账漂移这类 ERROR；
//! - 外送失败只在"通 → 不通"时记一条本地 ERROR，恢复时补发一条丢失数（不逐条刷屏）；
//! - 启动时试发一条（[`AlertHandle::probe`]），不通就拒绝启动；退出原因
//!   （[`AlertHandle::fatal`]）不占名额直接发；退出前 [`AlertHandle::flush`] 在限定时间内
//!   把队列和未报的汇总发完。这三步由 `engine::run_main` 统一调用。
//!
//! # 不拖垮引擎
//!
//! Layer 内只做同步格式化 + `unbounded_send`，不做任何 IO；发送任务自己的日志用
//! [`SELF_TARGET`]，Layer 精确排除，不会自激回环。
//!
//! # 为什么发送任务不挂在任务树上
//!
//! 它的一生必须盖住整棵树的一生：树根建立之前就要试发，停机途中各节点的 WARN/ERROR 要照发，
//! 根停完之后还要发退出原因。挂在树上就会先于最要紧的那几条被停掉。它的收尾由进程入口负责：
//! `engine::run_main` 在返回前用 [`AlertHandle::flush`] 限时等它把队列发完（进程正常退出、
//! 出错退出、主流程 panic 都走这一步），所以它不是无人等的游离任务。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// 本模块自己产生的日志所用 target：Layer 据此**精确匹配**排除，防自激回环。
/// 依赖库（hyper/rustls 等）因 webhook 失败产生的 WARN 仍会进来，由去重与名额兜住。
const SELF_TARGET: &str = "hft_alert_sender";

/// 去重与名额的窗口
const WINDOW: Duration = Duration::from_secs(300);

/// 每窗口外送名额（按级别分开算，互不挤占）
fn budget(level: Level) -> u32 {
    if level == Level::ERROR {
        30
    } else {
        10
    }
}

/// 汇总里每类告警的示例文本最多保留的字符数
const SAMPLE_CHARS: usize = 150;

/// 告警外送目的地
#[derive(Clone)]
pub enum AlertWebhookConfig {
    /// 通用 webhook（Slack incoming webhook 等）：POST `{"text": "..."}`
    Webhook { url: String },
    /// Slack bot：`chat.postMessage`，带 bot token、发到指定频道（bot 须已在该频道）
    SlackBot { token: String, channel: String },
}

/// 手写 Debug：token 不进日志
impl std::fmt::Debug for AlertWebhookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Webhook { url } => f.debug_struct("Webhook").field("url", url).finish(),
            Self::SlackBot { channel, .. } => f.debug_struct("SlackBot").field("channel", channel).finish(),
        }
    }
}

const SLACK_POST_MESSAGE: &str = "https://slack.com/api/chat.postMessage";

impl AlertWebhookConfig {
    /// 从环境变量装配，全部未设置即关闭（None）：
    /// - `SLACK_BOT_TOKEN` + `SLACK_ALERT_CHANNEL` → Slack bot（只设其一视为配置错，拒绝启动）
    /// - 否则 `ALERT_WEBHOOK_URL` → 通用 webhook
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        Self::from_vars(var("SLACK_BOT_TOKEN"), var("SLACK_ALERT_CHANNEL"), var("ALERT_WEBHOOK_URL"))
    }

    fn from_vars(token: Option<String>, channel: Option<String>, url: Option<String>) -> anyhow::Result<Option<Self>> {
        match (token, channel) {
            (Some(token), Some(channel)) => Ok(Some(Self::SlackBot { token, channel })),
            (None, None) => Ok(url.map(|url| Self::Webhook { url })),
            // 告警配一半等于没配，而运行者以为配了 —— 启动期就失败
            _ => anyhow::bail!("SLACK_BOT_TOKEN 与 SLACK_ALERT_CHANNEL 必须同时设置"),
        }
    }
}

/// 把一条文本送出去的通道。发送任务只认这个接口，测试用替身。
trait AlertSink: Send + Sync + 'static {
    fn send(&self, text: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

struct HttpSink {
    config: AlertWebhookConfig,
    client: reqwest::Client,
}

impl AlertSink for HttpSink {
    async fn send(&self, text: &str) -> Result<(), String> {
        match &self.config {
            AlertWebhookConfig::Webhook { url } => {
                crate::net::send_unmetered(self.client.post(url).json(&serde_json::json!({ "text": text })))
                    .await
                    .and_then(|r| r.error_for_status())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
            AlertWebhookConfig::SlackBot { token, channel } => {
                let resp: serde_json::Value = crate::net::send_unmetered(
                    self.client
                        .post(SLACK_POST_MESSAGE)
                        .bearer_auth(token)
                        .json(&serde_json::json!({ "channel": channel, "text": text })),
                )
                .await
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                // Slack 出错也回 HTTP 200，要看 ok 字段（如 not_in_channel / channel_not_found）
                if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                    Ok(())
                } else {
                    Err(format!("slack: {}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")))
                }
            }
        }
    }
}

/// 一条待外送的告警
struct Alert {
    level: Level,
    /// 去重键：日志调用点 `target:file:line`（经 `log` crate 转发的依赖库日志取其原始位置）
    key: String,
    text: String,
}

enum Msg {
    Alert(Alert),
    /// 立即试发一条（不占名额），回报结果
    Probe(oneshot::Sender<Result<(), String>>),
    /// 不占名额、不去重直接发（进程退出原因）
    Fatal(String),
    /// 把之前入队的全部处理完、未报的汇总发出后回报
    Flush(oneshot::Sender<()>),
}

/// 启动后台发送任务，返回要挂到 subscriber 上的 Layer 和给进程入口用的句柄。
/// 必须在 tokio runtime 内调用。
///
/// 用法（见 `engine::bootstrap::init_tracing`，两条 per-layer filter 缺一不可）：
/// `registry().with(fmt_layer.with_filter(env_filter))
///     .with(alert_layer.with_filter(LevelFilter::WARN)).init()`
/// —— EnvFilter 不能全局挂（会连带灭掉告警外送），告警层不能裸挂（无过滤器的 layer
/// 会把全局 max level hint 退化到 TRACE，依赖库的 trace 事件全部被构造派发）。
pub fn spawn_alert_webhook_layer(config: AlertWebhookConfig) -> anyhow::Result<(AlertWebhookLayer, AlertHandle)> {
    let client = crate::net::http(Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("告警发送器构建 HTTP client 失败: {e}"))?;
    Ok(spawn_with_sink(HttpSink { config, client }))
}

fn spawn_with_sink(sink: impl AlertSink) -> (AlertWebhookLayer, AlertHandle) {
    let (tx, rx) = mpsc::unbounded_channel::<Msg>();
    // 不挂任务树、收尾靠 run_main 的 flush：见模块文档「为什么发送任务不挂在任务树上」
    tokio::spawn(sender_task(sink, rx));
    (AlertWebhookLayer { tx: tx.clone() }, AlertHandle { tx })
}

/// 进程入口用的句柄：启动试发、退出前发完
pub struct AlertHandle {
    tx: mpsc::UnboundedSender<Msg>,
}

impl AlertHandle {
    /// 试发一条告警，返回外送是否打通
    pub async fn probe(&self) -> anyhow::Result<()> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Probe(reply));
        match rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::anyhow!("告警试发失败: {e}")),
            Err(_) => Err(anyhow::anyhow!("告警发送任务已退出")),
        }
    }

    /// 记本地 ERROR 并不占名额直接外送 —— 退出原因不能被窗口名额吞进汇总、截成半句
    pub fn fatal(&self, reason: &str) {
        tracing::error!(target: SELF_TARGET, reason, "进程因错误退出");
        let _ = self.tx.send(Msg::Fatal(format!("[{}][ERROR] 进程因错误退出: {reason}", instance_label())));
    }

    /// 在 `deadline` 内把已入队的告警和未报的汇总发完；超时返回 false
    pub async fn flush(&self, deadline: Duration) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Msg::Flush(reply)).is_err() {
            return false;
        }
        matches!(tokio::time::timeout(deadline, rx).await, Ok(Ok(())))
    }
}

/// 捕获 WARN/ERROR 事件推给后台发送任务的 tracing Layer
pub struct AlertWebhookLayer {
    tx: mpsc::UnboundedSender<Msg>,
}

impl<S: Subscriber> Layer<S> for AlertWebhookLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // 依赖库经 `log` crate 打的日志共用一个 "log event" 调用点，还原成它的真实位置
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        // Level 的序：ERROR < WARN < INFO（越严重越"小"）
        if *meta.level() > Level::WARN {
            return;
        }
        if meta.target() == SELF_TARGET {
            return;
        }
        let mut text = format!("[{}][{}] {}: ", instance_label(), meta.level(), meta.target());
        let mut visitor = TextVisitor(&mut text);
        event.record(&mut visitor);
        // 接收端关闭（发送任务退出）时丢弃 —— 告警通道绝不反向影响引擎
        let key = format!("{}:{}:{}", meta.target(), meta.file().unwrap_or("?"), meta.line().unwrap_or(0));
        let _ = self.tx.send(Msg::Alert(Alert { level: *meta.level(), key, text }));
    }
}

/// 实例标识（多实例部署时区分告警来源）：进程可执行文件名，取不到则 "hft"
fn instance_label() -> &'static str {
    use std::sync::OnceLock;
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "hft".to_string())
    })
}

/// 把事件字段拼成一行文本（message 在前，其余 `key=value`）
struct TextVisitor<'a>(&'a mut String);

impl Visit for TextVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?} ");
        } else if field.name().starts_with("log.") {
            // log 转发的位置字段，已体现在 target 与去重键里
        } else {
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }
}

/// 窗口内没外送的那部分：同一调用点被合并的次数和一条示例
struct Held {
    level: Level,
    sample: String,
    count: u64,
}

/// 去重 + 分级名额。纯状态，不碰 IO 和时钟（时间由调用方传入），便于测试。
struct Throttle {
    window_start: Instant,
    sent: HashMap<Level, u32>,
    /// 本窗口已外送过的调用点
    seen: std::collections::HashSet<String>,
    /// 本窗口被合并或超名额的调用点，按首次出现顺序
    held: Vec<(String, Held)>,
}

impl Throttle {
    fn new(now: Instant) -> Self {
        Self { window_start: now, sent: HashMap::new(), seen: Default::default(), held: Vec::new() }
    }

    /// 决定这条告警是否立即外送：返回 Some(文本) 就发，None 表示已计入汇总
    fn admit(&mut self, alert: Alert) -> Option<String> {
        let sent = self.sent.entry(alert.level).or_default();
        if !self.seen.contains(&alert.key) && *sent < budget(alert.level) {
            *sent += 1;
            self.seen.insert(alert.key);
            return Some(alert.text);
        }
        match self.held.iter_mut().find(|(k, _)| *k == alert.key) {
            Some((_, h)) => h.count += 1,
            None => self.held.push((
                alert.key,
                Held { level: alert.level, sample: alert.text.chars().take(SAMPLE_CHARS).collect(), count: 1 },
            )),
        }
        None
    }

    /// 窗口到期（或 `force`）时结束本窗口，返回要发的汇总（没有被合并的就是 None）
    fn roll(&mut self, now: Instant, force: bool) -> Option<String> {
        if !force && now.duration_since(self.window_start) < WINDOW {
            return None;
        }
        let held = std::mem::take(&mut self.held);
        *self = Self::new(now);
        if held.is_empty() {
            return None;
        }
        // ERROR 排前面
        let mut held = held;
        held.sort_by_key(|(_, h)| h.level != Level::ERROR);
        let mut text = format!("[{}] 告警汇总：以下告警最近一段时间重复或超出名额，未逐条外送", instance_label());
        for (_, h) in held {
            let _ = write!(text, "\n×{} {}", h.count, h.sample);
        }
        Some(text)
    }

    fn next_roll(&self) -> Instant {
        self.window_start + WINDOW
    }
}

/// 外送通断状态：只在状态变化时记日志
struct Health {
    /// Some(n) 表示当前不通，期间丢了 n 条
    down: Option<u64>,
}

async fn deliver(sink: &impl AlertSink, health: &mut Health, text: &str) {
    match sink.send(text).await {
        Ok(()) => {
            if let Some(lost) = health.down.take() {
                tracing::info!(target: SELF_TARGET, lost, "告警外送已恢复");
                let notice = format!("[{}] 告警外送曾中断，期间丢失 {lost} 条，详见本机日志", instance_label());
                if let Err(e) = sink.send(&notice).await {
                    tracing::error!(target: SELF_TARGET, error = %e, "告警外送不可用");
                    health.down = Some(lost);
                }
            }
        }
        Err(e) => match &mut health.down {
            Some(lost) => *lost += 1,
            None => {
                tracing::error!(target: SELF_TARGET, error = %e, "告警外送不可用，此后失败不再逐条记录，恢复时报丢失条数");
                health.down = Some(1);
            }
        },
    }
}

async fn sender_task(sink: impl AlertSink, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut throttle = Throttle::new(Instant::now());
    let mut health = Health { down: None };
    loop {
        let msg = tokio::select! {
            m = rx.recv() => match m { Some(m) => m, None => return },
            _ = tokio::time::sleep_until(throttle.next_roll()) => {
                if let Some(summary) = throttle.roll(Instant::now(), false) {
                    deliver(&sink, &mut health, &summary).await;
                }
                continue;
            }
        };
        match msg {
            Msg::Alert(alert) => {
                if let Some(text) = throttle.admit(alert) {
                    deliver(&sink, &mut health, &text).await;
                }
            }
            Msg::Fatal(text) => deliver(&sink, &mut health, &text).await,
            Msg::Probe(reply) => {
                let text = format!("[{}] 启动自检：告警外送可用", instance_label());
                let _ = reply.send(sink.send(&text).await);
            }
            Msg::Flush(reply) => {
                if let Some(summary) = throttle.roll(Instant::now(), true) {
                    deliver(&sink, &mut health, &summary).await;
                }
                let _ = reply.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    #[test]
    fn config_from_vars() {
        let s = |x: &str| Some(x.to_string());
        let ok = |t, c, u| AlertWebhookConfig::from_vars(t, c, u).unwrap();
        assert!(matches!(ok(s("t"), s("#c"), None), Some(AlertWebhookConfig::SlackBot { .. })));
        assert!(matches!(ok(None, None, s("http://x")), Some(AlertWebhookConfig::Webhook { .. })));
        assert!(ok(None, None, None).is_none());
        let c = ok(s("xoxb-secret"), s("#c"), None).unwrap();
        assert!(!format!("{c:?}").contains("xoxb-secret"));
        assert!(AlertWebhookConfig::from_vars(s("t"), None, None).is_err(), "Slack 配一半拒绝启动");
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Msg>) -> Vec<Alert> {
        let mut got = Vec::new();
        while let Ok(Msg::Alert(a)) = rx.try_recv() {
            got.push(a);
        }
        got
    }

    /// Layer 只捕 WARN/ERROR、排除发送器自身 target；文本含 message 与字段；按调用点给键
    #[test]
    fn layer_captures_warn_and_error_only() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let subscriber = tracing_subscriber::registry().with(AlertWebhookLayer { tx });

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "t", "不该外送");
            tracing::warn!(target: "t", symbol = "BTC", "腿不齐");
            tracing::error!(target: "t", "漂移确认");
            tracing::error!(target: "hft_alert_sender", "发送失败自身日志不回环");
        });
        let got = drain(&mut rx);
        assert_eq!(got.len(), 2, "只外送 WARN/ERROR 且排除自身");
        assert!(got[0].text.contains("腿不齐") && got[0].text.contains("symbol=\"BTC\""), "{}", got[0].text);
        assert!(got[1].text.contains("漂移确认"));
        assert_ne!(got[0].key, got[1].key, "不同调用点键不同");
    }

    /// 依赖库经 `log` crate 打的日志：去重键按各自原始位置区分，文本不带 log.* 位置字段
    #[test]
    fn log_crate_events_keyed_by_origin() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let subscriber = tracing_subscriber::registry().with(AlertWebhookLayer { tx });
        let rec = |line| {
            tracing_log::format_trace(
                &log::Record::builder()
                    .level(log::Level::Warn)
                    .target("tungstenite::protocol")
                    .file(Some("protocol.rs"))
                    .line(Some(line))
                    .args(format_args!("连接被重置"))
                    .build(),
            )
            .unwrap();
        };
        tracing::subscriber::with_default(subscriber, || {
            rec(10);
            rec(20);
        });
        let got = drain(&mut rx);
        assert_eq!(got.len(), 2);
        assert_ne!(got[0].key, got[1].key, "不同位置的依赖库日志不能共用一个键");
        assert!(got[0].text.contains("tungstenite::protocol") && !got[0].text.contains("log.file"), "{}", got[0].text);
    }

    /// **Critical 回归防线**：控制台的 EnvFilter 收紧（如 RUST_LOG=error）绝不能连带
    /// 关掉告警外送 —— 过滤必须是 fmt 层的 per-layer filter，不是全局 layer。
    /// 本测试按 init_tracing 的真实叠放结构构造 subscriber。
    #[test]
    fn console_filter_does_not_silence_alerts() {
        use tracing_subscriber::filter::EnvFilter;
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(EnvFilter::new("error")),
            )
            .with(AlertWebhookLayer { tx });

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "t", "控制台被 error 级过滤，但告警必须外送");
        });
        assert_eq!(drain(&mut rx).len(), 1, "RUST_LOG=error 把 WARN 告警外送也灭掉了");
    }

    fn alert(level: Level, key: &str, text: &str) -> Alert {
        Alert { level, key: key.to_string(), text: text.to_string() }
    }

    /// 同一调用点窗口内只发第一条，其余进汇总；WARN 用完名额不影响 ERROR
    #[test]
    fn throttle_dedups_and_keeps_error_budget() {
        let t0 = Instant::now();
        let mut th = Throttle::new(t0);
        assert!(th.admit(alert(Level::WARN, "w0", "首条")).is_some());
        assert!(th.admit(alert(Level::WARN, "w0", "重复")).is_none());
        // 用完 WARN 名额
        let keys: Vec<String> = (1..=20).map(|i| format!("w{i}")).collect();
        let sent_warn = keys.iter().filter(|k| th.admit(alert(Level::WARN, k, "别的 WARN")).is_some()).count();
        assert_eq!(sent_warn as u32, budget(Level::WARN) - 1);
        assert!(th.admit(alert(Level::ERROR, "e", "漂移确认")).is_some(), "ERROR 不被 WARN 挤掉");

        assert!(th.roll(t0 + WINDOW / 2, false).is_none(), "窗口未到不汇总");
        let summary = th.roll(t0 + WINDOW, false).expect("有被合并的就汇总");
        assert!(summary.contains("×1 重复"), "{summary}");
        assert!(th.roll(t0 + WINDOW * 3, false).is_none(), "汇总后清零");
        assert!(th.admit(alert(Level::WARN, "w0", "新窗口首条")).is_some(), "新窗口重新放行");
    }

    #[derive(Clone, Default)]
    struct FakeSink {
        ok: Arc<Mutex<bool>>,
        sent: Arc<Mutex<Vec<String>>>,
        attempts: Arc<Mutex<u32>>,
    }

    impl AlertSink for FakeSink {
        async fn send(&self, text: &str) -> Result<(), String> {
            *self.attempts.lock().unwrap() += 1;
            if *self.ok.lock().unwrap() {
                self.sent.lock().unwrap().push(text.to_string());
                Ok(())
            } else {
                Err("channel_not_found".into())
            }
        }
    }

    /// 在独立线程上的 subscriber 里收集发送任务自己的日志
    fn capture_self_logs() -> (impl Subscriber + Send + Sync, Arc<Mutex<Vec<String>>>) {
        #[derive(Clone)]
        struct W(Arc<Mutex<Vec<String>>>);
        impl std::io::Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().push(String::from_utf8_lossy(b).into_owned());
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Arc::new(Mutex::new(Vec::new()));
        let w = W(logs.clone());
        let sub = tracing_subscriber::fmt().with_ansi(false).with_writer(move || w.clone()).finish();
        (sub, logs)
    }

    /// 启动试发：不通返回错误；外送失败只在状态变化时记一次，恢复时补报丢失数；flush 发出汇总
    #[tokio::test]
    async fn probe_failure_transitions_and_flush() {
        let (sub, logs) = capture_self_logs();
        let _g = tracing::subscriber::set_default(sub);
        let sink = FakeSink::default();
        let (layer, handle) = spawn_with_sink(sink.clone());
        let send = |level: Level, key: &str, text: &str| {
            let _ = layer.tx.send(Msg::Alert(alert(level, key, text)));
        };

        assert!(handle.probe().await.is_err(), "不通时试发要报错");

        for k in ["a", "b", "c"] {
            send(Level::ERROR, k, "失败中");
        }
        assert!(handle.flush(Duration::from_secs(1)).await);
        let down_logs = logs.lock().unwrap().iter().filter(|l| l.contains("告警外送不可用")).count();
        assert_eq!(down_logs, 1, "连续失败只记一次");

        *sink.ok.lock().unwrap() = true;
        send(Level::ERROR, "d", "恢复后第一条");
        send(Level::ERROR, "d", "重复的");
        assert!(handle.flush(Duration::from_secs(1)).await);
        let sent = sink.sent.lock().unwrap().clone();
        assert!(sent[0].contains("恢复后第一条"));
        assert!(sent[1].contains("丢失 3 条"), "a b c 都丢了: {sent:?}");
        assert!(sent[2].contains("×1 重复的"), "flush 把汇总发出: {sent:?}");
        assert!(logs.lock().unwrap().iter().any(|l| l.contains("告警外送已恢复")));

        // 退出原因不占名额：ERROR 名额用完后照样直发
        for i in 0..budget(Level::ERROR) {
            send(Level::ERROR, &format!("e{i}"), "占名额");
        }
        handle.fatal("对账漂移");
        assert!(handle.flush(Duration::from_secs(1)).await);
        assert!(sink.sent.lock().unwrap().iter().any(|t| t.contains("进程因错误退出: 对账漂移")));
    }
}

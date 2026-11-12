//! 告警外送：tracing Layer 旁路 + 后台 webhook 发送任务。
//!
//! # 为什么挂在 tracing 上，而不是新建一条告警事件通道
//!
//! 引擎的全部告警点（对账漂移、投产失败、WS 断流 kill、单腿裸敞口……）都已经是
//! WARN/ERROR 级的结构化日志 —— 告警的本质就是"这些日志必须有人立刻看到"。挂 Layer
//! 零侵入地覆盖全部既有与将来的告警点；若另建通道，每个告警点都要记得多发一份，
//! 漏一处就是盲区。
//!
//! # 不拖垮引擎的三道保险
//!
//! - Layer 内只做同步格式化 + `unbounded_send`，不做任何 IO；
//! - 发送任务限流（每窗口至多 [`MAX_ALERTS_PER_WINDOW`] 条，超出丢弃并在下一条带上
//!   被抑制的数量）—— 告警风暴不会打爆 webhook 也不会堆积内存带宽；
//! - 发送失败只降级记录（target 已被本 Layer 排除，不会自激回环）。

use std::fmt::Write as _;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// 本模块自己产生的日志所用 target：Layer 据此**精确匹配**排除，防自激回环。
/// 注意排除只覆盖本 target —— webhook 失败若引发依赖库自身的 WARN（hyper/rustls 等），
/// 仍会进入告警通道，**窗口限流是回环的最终兜底**。
const SELF_TARGET: &str = "hft_alert_sender";

/// 限流窗口
const WINDOW: Duration = Duration::from_secs(60);
/// 每窗口最多外送的告警条数（超出丢弃，计入 suppressed）
const MAX_ALERTS_PER_WINDOW: u32 = 20;

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
    /// - `SLACK_BOT_TOKEN` + `SLACK_ALERT_CHANNEL` → Slack bot（只设其一视为配置错，panic 拒绝启动）
    /// - 否则 `ALERT_WEBHOOK_URL` → 通用 webhook
    pub fn from_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        Self::from_vars(var("SLACK_BOT_TOKEN"), var("SLACK_ALERT_CHANNEL"), var("ALERT_WEBHOOK_URL"))
    }

    fn from_vars(token: Option<String>, channel: Option<String>, url: Option<String>) -> Option<Self> {
        match (token, channel) {
            (Some(token), Some(channel)) => Some(Self::SlackBot { token, channel }),
            (None, None) => url.map(|url| Self::Webhook { url }),
            // 告警配一半等于没配，而运行者以为配了 —— 启动期就失败
            _ => panic!("SLACK_BOT_TOKEN 与 SLACK_ALERT_CHANNEL 必须同时设置"),
        }
    }

    /// 发一条告警
    async fn send(&self, client: &reqwest::Client, text: &str) -> Result<(), String> {
        match self {
            Self::Webhook { url } => client
                .post(url)
                .json(&serde_json::json!({ "text": text }))
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Self::SlackBot { token, channel } => {
                let resp: serde_json::Value = client
                    .post(SLACK_POST_MESSAGE)
                    .bearer_auth(token)
                    .json(&serde_json::json!({ "channel": channel, "text": text }))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                // Slack 出错也回 HTTP 200，要看 ok 字段（如 not_in_channel / invalid_auth）
                if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                    Ok(())
                } else {
                    Err(format!("slack: {}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")))
                }
            }
        }
    }
}

/// 启动后台发送任务并返回可挂载的 tracing Layer。
///
/// 用法（见 `engine::bootstrap::init_tracing`，两条 per-layer filter 缺一不可）：
/// `registry().with(fmt_layer.with_filter(env_filter))
///     .with(alert_layer.with_filter(LevelFilter::WARN)).init()`
/// —— EnvFilter 不能全局挂（会连带灭掉告警外送），告警层不能裸挂（无过滤器的 layer
/// 会把全局 max level hint 退化到 TRACE，依赖库的 trace 事件全部被构造派发）。
pub fn spawn_alert_webhook_layer(config: AlertWebhookConfig) -> AlertWebhookLayer {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    // 后台发送任务需要 tokio runtime；同步上下文调用不 panic，降级为禁用外送
    // （channel 无接收端，Layer 的 send 静默失败），并在 stderr 留一条可见提示。
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(sender_task(config, rx));
        }
        Err(_) => {
            eprintln!("告警外送已配置，但当前不在 tokio runtime 内，告警外送被禁用");
        }
    }
    AlertWebhookLayer { tx }
}

/// 捕获 WARN/ERROR 事件推给后台发送任务的 tracing Layer
pub struct AlertWebhookLayer {
    tx: mpsc::UnboundedSender<String>,
}

impl<S: Subscriber> Layer<S> for AlertWebhookLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Level 的序：ERROR < WARN < INFO（越严重越"小"）
        if *meta.level() > Level::WARN {
            return;
        }
        // 防自激回环：发送任务自己的失败日志不再外送（精确匹配即可，无别的 target 同名）
        if meta.target() == SELF_TARGET {
            return;
        }
        let mut text = format!("[{}][{}] {}: ", instance_label(), meta.level(), meta.target());
        let mut visitor = TextVisitor(&mut text);
        event.record(&mut visitor);
        // 接收端关闭（发送任务退出）时静默丢弃 —— 告警通道绝不反向影响引擎
        let _ = self.tx.send(text);
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
        } else {
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }
}

/// 后台发送：逐条 POST，按窗口限流
async fn sender_task(config: AlertWebhookConfig, mut rx: mpsc::UnboundedReceiver<String>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: SELF_TARGET, error = %e, "告警发送器构建 HTTP client 失败，告警外送不可用");
            return;
        }
    };
    let mut window_start = tokio::time::Instant::now();
    let mut sent_in_window: u32 = 0;
    // 已知取舍：抑制数随下一条告警补报 —— 风暴收尾后若再无新告警，最后一批抑制数
    // 不会单独上报（值班者已经收到了满窗的同类告警，损失可接受）
    let mut suppressed: u64 = 0;

    while let Some(mut text) = rx.recv().await {
        if window_start.elapsed() >= WINDOW {
            window_start = tokio::time::Instant::now();
            sent_in_window = 0;
        }
        if sent_in_window >= MAX_ALERTS_PER_WINDOW {
            suppressed += 1;
            continue;
        }
        sent_in_window += 1;
        if suppressed > 0 {
            let _ = write!(text, "（此前 {suppressed} 条告警因限流被抑制）");
            suppressed = 0;
        }
        if let Err(e) = config.send(&client, &text).await {
            // target 已被 Layer 排除，不会回环；持续失败的量受限流约束
            tracing::warn!(target: SELF_TARGET, error = %e, "告警外送失败，该条丢弃");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[test]
    fn config_from_vars() {
        let s = |x: &str| Some(x.to_string());
        assert!(matches!(AlertWebhookConfig::from_vars(s("t"), s("#c"), None), Some(AlertWebhookConfig::SlackBot { .. })));
        assert!(matches!(AlertWebhookConfig::from_vars(None, None, s("http://x")), Some(AlertWebhookConfig::Webhook { .. })));
        assert!(AlertWebhookConfig::from_vars(None, None, None).is_none());
        let c = AlertWebhookConfig::from_vars(s("xoxb-secret"), s("#c"), None).unwrap();
        assert!(!format!("{c:?}").contains("xoxb-secret"));
    }

    #[test]
    #[should_panic]
    fn half_slack_config_refuses_to_start() {
        AlertWebhookConfig::from_vars(Some("t".into()), None, None);
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut got = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            got.push(msg);
        }
        got
    }

    /// Layer 只捕 WARN/ERROR、排除发送器自身 target；文本含 message 与字段
    #[test]
    fn layer_captures_warn_and_error_only() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let layer = AlertWebhookLayer { tx };
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "t", "不该外送");
            tracing::warn!(target: "t", symbol = "BTC", "单腿裸敞口");
            tracing::error!(target: "t", "漂移确认");
            tracing::error!(target: "hft_alert_sender", "发送失败自身日志不回环");
        });
        let got = drain(&mut rx);
        assert_eq!(got.len(), 2, "只外送 WARN/ERROR 且排除自身: {got:?}");
        assert!(got[0].contains("单腿裸敞口") && got[0].contains("symbol=\"BTC\""), "{}", got[0]);
        assert!(got[1].contains("漂移确认"));
    }

    /// **Critical 回归防线**：控制台的 EnvFilter 收紧（如 RUST_LOG=error）绝不能连带
    /// 关掉告警外送 —— 过滤必须是 fmt 层的 per-layer filter，不是全局 layer。
    /// 本测试按 init_tracing 的真实叠放结构构造 subscriber。
    #[test]
    fn console_filter_does_not_silence_alerts() {
        use tracing_subscriber::filter::EnvFilter;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let layer = AlertWebhookLayer { tx };
        // 与 bootstrap::init_tracing 同构：fmt 层带 per-layer EnvFilter("error")，告警层独立
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(EnvFilter::new("error")),
            )
            .with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "t", "控制台被 error 级过滤，但告警必须外送");
        });
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "RUST_LOG=error 把 WARN 告警外送也灭掉了: {got:?}");
    }
}

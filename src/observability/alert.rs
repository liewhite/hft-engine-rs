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

/// 本模块自己产生的日志所用 target 前缀：Layer 据此排除，防自激回环
const SELF_TARGET: &str = "hft_alert_sender";

/// 限流窗口
const WINDOW: Duration = Duration::from_secs(60);
/// 每窗口最多外送的告警条数（超出丢弃，计入 suppressed）
const MAX_ALERTS_PER_WINDOW: u32 = 20;

/// 告警外送配置
#[derive(Debug, Clone)]
pub struct AlertWebhookConfig {
    /// webhook 端点（POST `{"text": "..."}`）
    pub url: String,
}

impl AlertWebhookConfig {
    /// 从环境变量装配：`ALERT_WEBHOOK_URL` 未设置即关闭（None）
    pub fn from_env() -> Option<Self> {
        std::env::var("ALERT_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|url| Self { url })
    }
}

/// 启动后台发送任务并返回可挂载的 tracing Layer。
///
/// 用法（见 `engine::bootstrap::init_tracing`）：
/// `registry().with(fmt_layer).with(alert_layer).with(filter).init()`
pub fn spawn_alert_webhook_layer(config: AlertWebhookConfig) -> AlertWebhookLayer {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(sender_task(config, rx));
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
        // 防自激回环：发送任务自己的失败日志不再外送
        if meta.target().starts_with(SELF_TARGET) {
            return;
        }
        let mut text = format!("[{}] {}: ", meta.level(), meta.target());
        let mut visitor = TextVisitor(&mut text);
        event.record(&mut visitor);
        // 接收端关闭（发送任务退出）时静默丢弃 —— 告警通道绝不反向影响引擎
        let _ = self.tx.send(text);
    }
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
        let body = serde_json::json!({ "text": text });
        if let Err(e) = client.post(&config.url).json(&body).send().await {
            // target 已被 Layer 排除，不会回环；持续失败的量受限流约束
            tracing::warn!(target: SELF_TARGET, error = %e, "告警外送失败，该条丢弃");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::prelude::*;

    /// Layer 只捕 WARN/ERROR、排除发送器自身 target；文本含 message 与字段
    #[test]
    fn layer_captures_warn_and_error_only() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let layer = AlertWebhookLayer { tx };
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "t", "不该外送");
            tracing::warn!(target: "t", symbol = "BTC", "单腿裸敞口");
            tracing::error!(target: "t", "漂移确认");
            tracing::error!(target: "hft_alert_sender", "发送失败自身日志不回环");
        });
        while let Ok(msg) = rx.try_recv() {
            captured.lock().unwrap().push(msg);
        }
        let got = captured.lock().unwrap();
        assert_eq!(got.len(), 2, "只外送 WARN/ERROR 且排除自身: {got:?}");
        assert!(got[0].contains("单腿裸敞口") && got[0].contains("symbol=\"BTC\""), "{}", got[0]);
        assert!(got[1].contains("漂移确认"));
    }
}

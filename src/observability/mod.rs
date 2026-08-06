//! 可观测性出口：把引擎内部已有的结构化信号送到外部系统。
//!
//! 核心原则：**未配置观测出口时，引擎行为与没有本模块完全一致**（历史上曾内置
//! pushgateway + Slack，为保持纯策略框架而删除，见 docs/todo.md）。如实说明依赖方向：
//! 编译期是引擎引用本模块（metrics 填充推送文本、bootstrap 挂告警层），本模块不反向
//! 依赖引擎运行时；运行期出口全部旁路（异步、失败降级、限流），绝不拖垮交易。
//! - 告警外送（[`alert`]）：tracing 的 WARN/ERROR 事件经 Layer 旁路推 webhook ——
//!   零业务侵入，对账漂移/投产失败/断流退出等既有告警点自动覆盖
//! - 指标推送（[`prometheus`]）：`MetricsActor` 的周期快照可选推 Prometheus pushgateway
//!
//! 两者都由环境变量启用（未设置 = 关闭，行为与从前完全一致）：
//! - `ALERT_WEBHOOK_URL`：告警 webhook 端点（POST `{"text": "..."}`，兼容 Slack
//!   incoming webhook / 飞书自定义机器人的 text 模式）
//! - `PUSHGATEWAY_URL` / `PUSHGATEWAY_JOB`（默认 `hft_engine`）：指标推送端点。
//!   **多实例（分桶）部署必须各配不同的 `PUSHGATEWAY_JOB`** —— pushgateway 按 job
//!   分组，同 job 的推送互相覆盖

pub mod alert;
pub mod prometheus;

pub use alert::{spawn_alert_webhook_layer, AlertWebhookConfig};
pub use prometheus::{MetricsPushConfig, PromText};

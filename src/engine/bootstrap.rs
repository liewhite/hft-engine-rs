use kameo::actor::ActorRef;
use serde::de::DeserializeOwned;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::engine::live::ManagerActor;

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
    // 告警外送随控制台冗余度配置静默失效。告警层自带 WARN 级判断，不受控制台过滤影响。
    tracing_subscriber::registry()
        .with(fmt::layer().with_filter(filter))
        .with(alert_layer)
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

/// 等待 Ctrl+C 或 Manager 意外退出
pub async fn wait_for_shutdown(manager: ActorRef<ManagerActor>) {
    tracing::info!("System running. Press Ctrl+C to stop.");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal");
            if let Err(e) = manager.stop_gracefully().await {
                tracing::warn!(error = %e, "Failed to stop manager gracefully");
            }
        }
        _ = manager.wait_for_shutdown() => {
            tracing::error!("Manager actor died unexpectedly, exiting");
        }
    }

    tracing::info!("System stopped");
}

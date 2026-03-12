use kameo::actor::ActorRef;
use serde::de::DeserializeOwned;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::engine::live::ManagerActor;

/// 初始化 tracing（fmt + EnvFilter，默认 fee_arb=info）
pub fn init_tracing() -> anyhow::Result<()> {
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new("fee_arb=info")
    };
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
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

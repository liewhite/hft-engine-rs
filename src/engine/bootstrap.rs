use kameo::actor::{ActorRef, Spawn};
use kameo::mailbox;
use serde::de::DeserializeOwned;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::domain::Symbol;
use crate::engine::config::{DatabaseConfig, MonitoringConfig};
use crate::engine::live::{
    ManagerActor, SubscribeIncome, SubscribeOutcome,
};
use crate::strategy::{
    MetricsSubscriberActor, MetricsSubscriberArgs, SlackNotifierActor, SlackNotifierArgs,
    SpreadArbStatsActor, SpreadArbStatsArgs,
};

/// 初始化 tracing（fmt + EnvFilter，默认 fee_arb=info）
pub fn init_tracing() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("fee_arb=info".parse()?))
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

/// 初始化监控 subscribers（Metrics + Slack）并订阅到 Manager
pub async fn init_monitoring(
    manager: &ActorRef<ManagerActor>,
    monitoring: &MonitoringConfig,
) -> anyhow::Result<()> {
    let metrics_subscriber = MetricsSubscriberActor::spawn_with_mailbox(
        MetricsSubscriberArgs {
            pushgateway_url: monitoring.pushgateway_url.clone(),
            metric_prefix: monitoring.metric_prefix.clone(),
            push_interval_ms: monitoring.push_interval_ms,
        },
        mailbox::unbounded(),
    );

    manager
        .tell(SubscribeIncome(metrics_subscriber))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe MetricsSubscriber to income: {}", e))?;
    tracing::info!("MetricsSubscriberActor created and subscribed");

    let slack_notifier = SlackNotifierActor::spawn_with_mailbox(
        SlackNotifierArgs {
            channel: monitoring.slack_channel.clone(),
            token: monitoring.slack_token.clone(),
        },
        mailbox::unbounded(),
    );

    manager
        .tell(SubscribeIncome(slack_notifier.clone()))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe SlackNotifier to income: {}", e))?;

    manager
        .tell(SubscribeOutcome(slack_notifier))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe SlackNotifier to outcome: {}", e))?;
    tracing::info!("SlackNotifierActor created and subscribed");

    Ok(())
}

/// 初始化 SpreadArb 统计 actor 并订阅到 Manager
pub async fn init_spread_arb_stats(
    manager: &ActorRef<ManagerActor>,
    symbols: impl IntoIterator<Item = Symbol>,
    db_config: &DatabaseConfig,
) -> anyhow::Result<()> {
    let db = crate::db::init_db(&db_config.url).await?;

    let stats_actor = SpreadArbStatsActor::spawn_with_mailbox(
        SpreadArbStatsArgs {
            symbols: symbols.into_iter().collect(),
            db,
        },
        mailbox::unbounded(),
    );

    manager
        .tell(SubscribeIncome(stats_actor.clone()))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe SpreadArbStats to income: {}", e))?;
    manager
        .tell(SubscribeOutcome(stats_actor))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe SpreadArbStats to outcome: {}", e))?;
    tracing::info!("SpreadArbStatsActor created and subscribed");

    Ok(())
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

use fee_arb::engine::{
    AddStrategies, DatabaseConfig, ManagerActor, ManagerActorArgs, MonitoringConfig,
    SubscribeIncome, SubscribeOutcome,
};
use fee_arb::exchange::hyperliquid::HyperliquidCredentials;
use fee_arb::exchange::ibkr::IbkrCredentials;
use fee_arb::strategy::{
    MetricsSubscriberActor, MetricsSubscriberArgs, SlackNotifierActor, SlackNotifierArgs,
    SpreadArbConfig, SpreadArbStatsActor, SpreadArbStatsArgs, SpreadArbStrategy,
};
use kameo::actor::Spawn;
use kameo::mailbox;
use serde::Deserialize;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// SpreadArb 独立配置
#[derive(Debug, Clone, Deserialize)]
struct SpreadArbAppConfig {
    ibkr: IbkrCredentials,
    hyperliquid: HyperliquidCredentials,
    strategy: SpreadArbConfig,
    monitoring: Option<MonitoringConfig>,
    database: Option<DatabaseConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("fee_arb=info".parse()?))
        .init();

    tracing::info!("SpreadArb system starting...");

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "spread_arb_config.json".to_string());
    tracing::info!(path = %config_path, "Loading config");

    let content = std::fs::read_to_string(&config_path)?;
    let config: SpreadArbAppConfig = serde_json::from_str(&content)?;

    // ManagerActor — 仅 IBKR + Hyperliquid
    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: None,
            okx_credentials: None,
            hyperliquid_credentials: Some(config.hyperliquid.clone()),
            ibkr_credentials: Some(config.ibkr.clone()),
        },
        mailbox::unbounded(),
    );

    // 为每个 symbol 创建 SpreadArbStrategy
    let strategies: Vec<Box<dyn fee_arb::strategy::Strategy>> = config
        .strategy
        .symbols
        .iter()
        .map(|symbol| {
            Box::new(SpreadArbStrategy::new(
                config.strategy.clone(),
                symbol.clone(),
            )) as Box<dyn fee_arb::strategy::Strategy>
        })
        .collect();

    let strategy_count = strategies.len();

    manager
        .ask(AddStrategies(strategies))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!(count = strategy_count, "SpreadArb strategies added");

    // 监控 subscribers
    if let Some(ref monitoring) = config.monitoring {
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
            .expect("Failed to subscribe MetricsSubscriberActor");
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
            .expect("Failed to subscribe SlackNotifierActor to income");
        manager
            .tell(SubscribeOutcome(slack_notifier))
            .send()
            .await
            .expect("Failed to subscribe SlackNotifierActor to outcome");
        tracing::info!("SlackNotifierActor created and subscribed");
    }

    // SpreadArb 统计 + DB 持久化
    if config.database.is_none() {
        tracing::warn!("database is not set, signals/orders/fills will not be persisted");
    }
    if let Some(ref db_config) = config.database {
        let db = fee_arb::db::init_db(&db_config.url).await?;

        let stats_actor = SpreadArbStatsActor::spawn_with_mailbox(
            SpreadArbStatsArgs {
                symbols: config.strategy.symbols.iter().cloned().collect(),
                db,
            },
            mailbox::unbounded(),
        );

        manager
            .tell(SubscribeIncome(stats_actor.clone()))
            .send()
            .await
            .expect("Failed to subscribe SpreadArbStatsActor to income");
        manager
            .tell(SubscribeOutcome(stats_actor))
            .send()
            .await
            .expect("Failed to subscribe SpreadArbStatsActor to outcome");
        tracing::info!("SpreadArbStatsActor created and subscribed");
    }

    tracing::info!("System running. Press Ctrl+C to stop.");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal");
            manager.stop_gracefully().await.ok();
        }
        _ = manager.wait_for_shutdown() => {
            tracing::error!("Manager actor died unexpectedly, exiting");
        }
    }

    tracing::info!("System stopped");
    Ok(())
}

use fee_arb::domain::{Exchange, Symbol};
use fee_arb::engine::{AddStrategy, ManagerActor, ManagerActorArgs, SubscribeIncome};
use fee_arb::exchange::binance::BinanceCredentials;
use fee_arb::exchange::hyperliquid::HyperliquidCredentials;
use fee_arb::exchange::okx::OkxCredentials;
use fee_arb::strategy::{
    FundingArbConfig, FundingArbStrategy, MetricsSubscriberActor, MetricsSubscriberArgs,
    SlackNotifierActor, SlackNotifierArgs,
};
use kameo::actor::Spawn;
use kameo::mailbox;
use serde::Deserialize;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// 交易所配置
#[derive(Debug, Clone, Deserialize)]
struct ExchangesConfig {
    binance: BinanceCredentials,
    okx: OkxCredentials,
    hyperliquid: HyperliquidCredentials,
}

impl ExchangesConfig {
    fn enabled_exchanges(&self) -> Vec<Exchange> {
        let exchanges = vec![Exchange::Binance, Exchange::OKX, Exchange::Hyperliquid];
        exchanges
    }
}

/// 策略配置
#[derive(Debug, Clone, Deserialize)]
struct StrategyConfig {
    symbols: Vec<String>,
    funding_arb: FundingArbConfig,
}

/// 监控配置
#[derive(Debug, Clone, Deserialize)]
pub struct MonitoringConfig {
    /// Prometheus Pushgateway URL
    pub pushgateway_url: String,
    /// Metric 前缀
    pub metric_prefix: String,
    /// 推送间隔（毫秒）
    pub push_interval_ms: u64,
    /// Slack channel
    pub slack_channel: String,
    /// Slack token
    pub slack_token: String,
}

impl StrategyConfig {
    fn parse_symbols(&self) -> Vec<Symbol> {
        self.symbols
            .iter()
            .filter_map(|s| Symbol::from_canonical(s))
            .collect()
    }
}

/// 完整配置
#[derive(Debug, Clone, Deserialize)]
struct Config {
    exchanges: ExchangesConfig,
    strategy: StrategyConfig,
    monitoring: Option<MonitoringConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("fee_arb=info".parse()?))
        .init();

    tracing::info!("Fee arbitrage system starting...");

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.json".to_string());
    tracing::info!(path = %config_path, "Loading config");

    let content = std::fs::read_to_string(&config_path)?;
    let config: Config = serde_json::from_str(&content)?;

    let symbols = config.strategy.parse_symbols();
    if symbols.is_empty() {
        anyhow::bail!("No valid symbols configured");
    }

    tracing::info!(symbols = ?symbols, "Configured symbols");

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: Some(config.exchanges.binance.clone()),
            okx_credentials: Some(config.exchanges.okx.clone()),
            hyperliquid_credentials: Some(config.exchanges.hyperliquid.clone()),
        },
        mailbox::unbounded(),
    );

    let enabled_exchanges = config.exchanges.enabled_exchanges();
    for symbol in symbols {
        let strategy = FundingArbStrategy::new(
            config.strategy.funding_arb.clone(),
            enabled_exchanges.clone(),
            symbol.clone(),
        );

        manager
            .ask(AddStrategy(Box::new(strategy)))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

        tracing::info!(symbol = %symbol, "Strategy added");
    }

    // 初始化监控 subscribers（如果配置了）
    if let Some(ref monitoring) = config.monitoring {
        // 创建 MetricsSubscriberActor
        let metrics_subscriber = MetricsSubscriberActor::spawn_with_mailbox(
            MetricsSubscriberArgs {
                pushgateway_url: monitoring.pushgateway_url.clone(),
                metric_prefix: monitoring.metric_prefix.clone(),
                push_interval_ms: monitoring.push_interval_ms,
            },
            mailbox::unbounded(),
        );

        // 订阅 Income 事件
        manager
            .tell(SubscribeIncome(metrics_subscriber))
            .send()
            .await
            .ok();
        tracing::info!("MetricsSubscriberActor created and subscribed");

        // 创建 SlackNotifierActor
        let slack_notifier = SlackNotifierActor::spawn_with_mailbox(
            SlackNotifierArgs {
                channel: monitoring.slack_channel.clone(),
                token: monitoring.slack_token.clone(),
            },
            mailbox::unbounded(),
        );

        // 订阅 Income 事件（用于监听订单成交）
        manager
            .tell(SubscribeIncome(slack_notifier))
            .send()
            .await
            .ok();
        tracing::info!("SlackNotifierActor created and subscribed");
    }

    tracing::info!("System running. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");

    tracing::info!("Received shutdown signal");
    manager.stop_gracefully().await.ok();
    tracing::info!("Manager stopped");

    Ok(())
}

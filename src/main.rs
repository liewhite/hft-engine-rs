use std::collections::{HashMap, HashSet};

use fee_arb::domain::{Exchange, Symbol, SymbolMeta};
use fee_arb::engine::{AddStrategies, GetAllSymbolMetas, ManagerActor, ManagerActorArgs, SubscribeIncome, SubscribeOutcome};
use fee_arb::exchange::binance::BinanceCredentials;
use fee_arb::exchange::hyperliquid::HyperliquidCredentials;
use fee_arb::exchange::okx::OkxCredentials;
use fee_arb::strategy::{
    FundingArbConfig, FundingArbStrategy, MetricsSubscriberActor, MetricsSubscriberArgs,
    SlackNotifierActor, SlackNotifierArgs,
};
use kameo::actor::Spawn;
use kameo::mailbox;
use md5::{Md5, Digest};
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

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: Some(config.exchanges.binance.clone()),
            okx_credentials: Some(config.exchanges.okx.clone()),
            hyperliquid_credentials: Some(config.exchanges.hyperliquid.clone()),
        },
        mailbox::unbounded(),
    );

    // 获取所有交易所的 symbol metas
    let metas: HashMap<Exchange, Vec<SymbolMeta>> = manager
        .ask(GetAllSymbolMetas)
        .send()
        .await?;

    // 计算所有交易所 symbol 的并集
    let all_symbols: HashSet<Symbol> = metas
        .values()
        .flat_map(|metas| metas.iter().map(|m| m.symbol.clone()))
        .collect();

    tracing::info!(total_symbols = all_symbols.len(), "Total unique symbols from all exchanges");

    // 对 symbol 做 MD5 取模 4，选取余数为 0 的（1/4 的 symbol）
    let symbols: Vec<Symbol> = all_symbols
        .into_iter()
        .filter(|symbol| {
            let mut hasher = Md5::new();
            hasher.update(symbol.as_bytes());
            let hash = hasher.finalize();
            // 取 MD5 哈希的最后一个字节取模 4
            let remainder = hash[15] % 4;
            remainder == 0
        })
        .collect();

    if symbols.is_empty() {
        anyhow::bail!("No symbols selected after MD5 filtering");
    }

    tracing::info!(selected_symbols = symbols.len(), "Symbols selected (MD5 mod 4 == 0)");


    let enabled_exchanges = config.exchanges.enabled_exchanges();

    // 批量创建所有策略
    let strategies: Vec<Box<dyn fee_arb::strategy::Strategy>> = symbols
        .iter()
        .map(|symbol| {
            Box::new(FundingArbStrategy::new(
                config.strategy.funding_arb.clone(),
                enabled_exchanges.clone(),
                symbol.clone(),
            )) as Box<dyn fee_arb::strategy::Strategy>
        })
        .collect();

    let strategy_count = strategies.len();

    // 批量添加策略
    manager
        .ask(AddStrategies(strategies))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!(count = strategy_count, "Strategies batch added");

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
            .expect("Failed to subscribe MetricsSubscriberActor to income events");
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
            .tell(SubscribeIncome(slack_notifier.clone()))
            .send()
            .await
            .expect("Failed to subscribe SlackNotifierActor to income events");

        // 订阅 Outcome 事件（用于监听下单信号）
        manager
            .tell(SubscribeOutcome(slack_notifier))
            .send()
            .await
            .expect("Failed to subscribe SlackNotifierActor to outcome events");
        tracing::info!("SlackNotifierActor created and subscribed");
    }

    tracing::info!("System running. Press Ctrl+C to stop.");

    // 同时等待 Ctrl+C 或 Manager 死亡
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

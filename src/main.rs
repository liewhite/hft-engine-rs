use fee_arb::config::{load_config, AppConfig};
use fee_arb::domain::Symbol;
use fee_arb::engine::{AddStrategy, ManagerActor, ManagerActorArgs};
use fee_arb::strategy::{FundingArbConfig, FundingArbStrategy};
use kameo::request::MessageSend;
use serde::Deserialize;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// 策略配置
#[derive(Debug, Clone, Deserialize)]
struct StrategyConfig {
    symbols: Vec<String>,
    funding_arb: FundingArbConfig,
}

impl StrategyConfig {
    fn parse_symbols(&self) -> Vec<Symbol> {
        self.symbols
            .iter()
            .filter_map(|s| Symbol::from_canonical(s))
            .collect()
    }
}

/// 完整配置（框架 + 策略）
#[derive(Debug, Clone, Deserialize)]
struct Config {
    #[serde(flatten)]
    app: AppConfig,
    strategy: StrategyConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("fee_arb=info".parse()?))
        .init();

    tracing::info!("Fee arbitrage system starting...");

    // Load configuration from config.json (or specified path)
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.json".to_string());
    tracing::info!(path = %config_path, "Loading config from file");
    let config: Config = load_config(&config_path)?;

    let symbols = config.strategy.parse_symbols();
    if symbols.is_empty() {
        anyhow::bail!("No valid symbols configured");
    }

    tracing::info!(symbols = ?symbols, "Configured symbols");

    // Create ManagerActor
    let manager = ManagerActor::new(ManagerActorArgs {
        binance_credentials: Some(config.app.exchanges.binance.clone()),
        okx_credentials: Some(config.app.exchanges.okx.clone()),
        hyperliquid_credentials: config.app.exchanges.hyperliquid.clone(),
    })
    .await;

    // Add strategies
    let enabled_exchanges = config.app.exchanges.enabled_exchanges();
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

    tracing::info!("All strategies added. System running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");

    tracing::info!("Received shutdown signal");

    // Graceful shutdown
    manager.stop_gracefully().await.ok();

    tracing::info!("Manager stopped");

    Ok(())
}

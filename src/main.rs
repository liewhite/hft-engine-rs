use fee_arb::config::AppConfig;
use fee_arb::engine::FundingEngine;
use fee_arb::strategy::FundingArbStrategy;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("fee_arb=info".parse()?))
        .init();

    tracing::info!("Fee arbitrage system starting...");

    // Load configuration from config.json (or specified path)
    let config_path = std::env::args().nth(1).unwrap_or_else(|| "config.json".to_string());
    tracing::info!(path = %config_path, "Loading config from file");
    let config = AppConfig::from_file(&config_path)?;

    let symbols = config.parse_symbols();
    if symbols.is_empty() {
        anyhow::bail!("No valid symbols configured");
    }

    tracing::info!(symbols = ?symbols, "Configured symbols");

    // Create and configure engine (exchanges will be initialized on run based on strategy requirements)
    let mut engine = FundingEngine::new(config.exchanges.clone(), config.engine.metrics.clone());

    // Create per-symbol strategies
    let exchanges = config.exchanges.enabled_exchanges();
    for symbol in symbols {
        let strategy = FundingArbStrategy::new(
            config.strategy.funding_arb.clone().into(),
            exchanges.clone(),
            symbol,
        );
        engine.add_strategy(strategy);
    }

    // Start engine
    engine.run().await?;

    tracing::info!("System running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    engine.wait_for_shutdown().await;

    // Graceful shutdown
    engine.stop();

    tracing::info!("System stopped.");

    Ok(())
}

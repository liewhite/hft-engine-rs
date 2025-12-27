use fee_arb::config::AppConfig;
use fee_arb::engine::{AddStrategy, EngineActor, EngineActorArgs, Start};
use fee_arb::strategy::FundingArbStrategy;
use kameo::request::MessageSend;
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

    // Create EngineActor
    let engine = kameo::spawn(EngineActor::new(EngineActorArgs {
        exchanges_config: config.exchanges.clone(),
    }));

    // Add strategies
    let exchanges = config.exchanges.enabled_exchanges();
    for symbol in symbols {
        let strategy = FundingArbStrategy::new(
            config.strategy.funding_arb.clone().into(),
            exchanges.clone(),
            symbol,
        );
        let _ = engine.tell(AddStrategy(Box::new(strategy))).await;
    }

    // Start engine
    engine
        .ask(Start)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!("System running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");

    tracing::info!("Received shutdown signal");

    // Graceful shutdown
    engine.stop_gracefully().await.ok();

    tracing::info!("Engine stopped");

    tracing::info!("System stopped.");

    Ok(())
}

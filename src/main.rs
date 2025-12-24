use fee_arb::config::AppConfig;
use fee_arb::engine::Coordinator;
use fee_arb::exchange::binance::BinanceWebSocket;
use fee_arb::exchange::okx::OkxWebSocket;
use fee_arb::strategy::FundingArbStrategy;
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("fee_arb=info".parse()?))
        .init();

    tracing::info!("Fee arbitrage system starting...");

    // Load configuration
    let config = match std::env::args().nth(1) {
        Some(path) => {
            tracing::info!(path = %path, "Loading config from file");
            AppConfig::from_file(&path)?
        }
        None => {
            tracing::info!("Loading config from environment variables");
            AppConfig::from_env()?
        }
    };

    let symbols = config.parse_symbols();
    if symbols.is_empty() {
        anyhow::bail!("No valid symbols configured");
    }

    tracing::info!(symbols = ?symbols, "Configured symbols");

    // Create shared exchange adapters
    let binance = Arc::new(BinanceWebSocket::new(
        config.exchanges.binance.api_key.clone(),
        config.exchanges.binance.secret.clone(),
    )?);

    let okx = Arc::new(OkxWebSocket::new(
        config.exchanges.okx.api_key.clone(),
        config.exchanges.okx.secret.clone(),
        config.exchanges.okx.passphrase.clone(),
    )?);

    // Create strategy with shared exchange instances
    let strategy = FundingArbStrategy::new(
        config.strategy.funding_arb.clone().into(),
        binance.clone(),
        okx.clone(),
    );

    // Create and start coordinator with shared exchange instances
    let mut coordinator = Coordinator::new(binance, okx, strategy, symbols);

    coordinator.start().await?;

    tracing::info!("System running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    coordinator.wait_for_shutdown().await;

    // Graceful shutdown
    coordinator.stop().await;

    tracing::info!("System stopped.");

    Ok(())
}

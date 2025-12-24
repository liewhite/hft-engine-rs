use fee_arb::config::AppConfig;
use fee_arb::domain::Exchange;
use fee_arb::engine::Engine;
use fee_arb::exchange::binance::BinanceWebSocket;
use fee_arb::exchange::okx::OkxWebSocket;
use fee_arb::exchange::ExchangeWebSocket;
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

    // Create WebSocket clients
    let binance_ws: Arc<dyn ExchangeWebSocket> = Arc::new(BinanceWebSocket::new(
        config.exchanges.binance.api_key.clone(),
        config.exchanges.binance.secret.clone(),
    )?);

    let okx_ws: Arc<dyn ExchangeWebSocket> = Arc::new(OkxWebSocket::new(
        config.exchanges.okx.api_key.clone(),
        config.exchanges.okx.secret.clone(),
        config.exchanges.okx.passphrase.clone(),
    )?);

    // Create strategy
    let strategy = FundingArbStrategy::new(
        config.strategy.funding_arb.clone().into(),
        vec![Exchange::Binance, Exchange::OKX],
        symbols,
    );

    // Create and configure engine
    let mut engine = Engine::new();
    engine.register_exchange(binance_ws);
    engine.register_exchange(okx_ws);
    engine.add_strategy(strategy);

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

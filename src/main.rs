use fee_arb::config::AppConfig;
use fee_arb::engine::{AddStrategy, ManagerActor, ManagerActorArgs};
use fee_arb::exchange::binance::{BinanceCredentials, BinanceModule};
use fee_arb::exchange::okx::{OkxCredentials, OkxModule};
use fee_arb::exchange::ExchangeModule;
use fee_arb::strategy::FundingArbStrategy;
use kameo::request::MessageSend;
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

    // Load configuration from config.json (or specified path)
    let config_path = std::env::args().nth(1).unwrap_or_else(|| "config.json".to_string());
    tracing::info!(path = %config_path, "Loading config from file");
    let config = AppConfig::from_file(&config_path)?;

    let symbols = config.parse_symbols();
    if symbols.is_empty() {
        anyhow::bail!("No valid symbols configured");
    }

    tracing::info!(symbols = ?symbols, "Configured symbols");

    // Create exchange modules (for REST client access)
    let mut modules: Vec<Arc<dyn ExchangeModule>> = Vec::new();

    // Binance module
    let binance_credentials = BinanceCredentials {
        api_key: config.exchanges.binance.api_key.clone(),
        secret: config.exchanges.binance.secret.clone(),
    };
    let binance_module = BinanceModule::new(Some(binance_credentials))?;
    modules.push(Arc::new(binance_module));

    // OKX module
    let okx_credentials = OkxCredentials {
        api_key: config.exchanges.okx.api_key.clone(),
        secret: config.exchanges.okx.secret.clone(),
        passphrase: config.exchanges.okx.passphrase.clone(),
    };
    let okx_module = OkxModule::new(Some(okx_credentials))?;
    modules.push(Arc::new(okx_module));

    // Create ManagerActor (ExchangeActors will be lazy spawned with spawn_link)
    let manager = kameo::spawn(ManagerActor::new(ManagerActorArgs { modules }));

    // Add strategies
    let enabled_exchanges = config.exchanges.enabled_exchanges();
    for symbol in symbols {
        let strategy = FundingArbStrategy::new(
            config.strategy.funding_arb.clone().into(),
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

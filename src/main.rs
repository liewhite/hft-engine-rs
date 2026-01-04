use fee_arb::config::AppConfig;
use fee_arb::domain::Exchange;
use fee_arb::engine::{AddStrategy, ManagerActor, ManagerActorArgs};
use fee_arb::exchange::binance::{
    BinanceActor, BinanceActorArgs, BinanceCredentials, BinanceModule, REST_BASE_URL,
};
use fee_arb::exchange::okx::{OkxActor, OkxActorArgs, OkxCredentials, OkxModule};
use fee_arb::exchange::{ExchangeActorOps, ExchangeModule};
use fee_arb::strategy::FundingArbStrategy;
use kameo::request::MessageSend;
use std::collections::HashMap;
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
    let binance_module = BinanceModule::new(Some(binance_credentials.clone()))?;
    modules.push(Arc::new(binance_module));

    // OKX module
    let okx_credentials = OkxCredentials {
        api_key: config.exchanges.okx.api_key.clone(),
        secret: config.exchanges.okx.secret.clone(),
        passphrase: config.exchanges.okx.passphrase.clone(),
    };
    let okx_module = OkxModule::new(Some(okx_credentials.clone()))?;
    modules.push(Arc::new(okx_module));

    // Spawn ExchangeActors (WebSocket 将在第一次 Subscribe 时懒创建)
    let mut exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>> = HashMap::new();

    // Binance Actor
    let binance_actor = kameo::spawn(BinanceActor::new(BinanceActorArgs {
        credentials: Some(binance_credentials),
        symbol_metas: Arc::new(HashMap::new()), // 会在 ManagerActor 中通过 symbol fetch 填充
        rest_base_url: REST_BASE_URL.to_string(),
    }));
    exchange_actors.insert(Exchange::Binance, Box::new(binance_actor));

    // OKX Actor
    let okx_actor = kameo::spawn(OkxActor::new(OkxActorArgs {
        credentials: Some(okx_credentials),
        symbol_metas: Arc::new(HashMap::new()),
    }));
    exchange_actors.insert(Exchange::OKX, Box::new(okx_actor));

    // Create ManagerActor
    let manager = kameo::spawn(ManagerActor::new(ManagerActorArgs {
        modules,
        exchange_actors,
    }));

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

use fee_arb::domain::{Exchange, Symbol};
use fee_arb::engine::{AddStrategy, ManagerActor, ManagerActorArgs};
use fee_arb::exchange::binance::BinanceCredentials;
use fee_arb::exchange::hyperliquid::HyperliquidCredentials;
use fee_arb::exchange::okx::OkxCredentials;
use fee_arb::strategy::{FundingArbConfig, FundingArbStrategy};
use kameo::request::MessageSend;
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

    let manager = ManagerActor::new(ManagerActorArgs {
        binance_credentials: Some(config.exchanges.binance.clone()),
        okx_credentials: Some(config.exchanges.okx.clone()),
        hyperliquid_credentials: Some(config.exchanges.hyperliquid.clone()),
    })
    .await;

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

    tracing::info!("System running. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for ctrl-c");

    tracing::info!("Received shutdown signal");
    manager.stop_gracefully().await.ok();
    tracing::info!("Manager stopped");

    Ok(())
}

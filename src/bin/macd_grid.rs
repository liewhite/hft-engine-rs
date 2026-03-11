use fee_arb::engine::{
    init_tracing, load_config, wait_for_shutdown, AddStrategies, ManagerActor, ManagerActorArgs,
};
use fee_arb::exchange::okx::OkxCredentials;
use fee_arb::strategy::{MacdGridConfig, MacdGridStrategy};
use kameo::actor::Spawn;
use kameo::mailbox;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct Config {
    okx: OkxCredentials,
    strategy: MacdGridConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    tracing::info!("MACD Grid strategy starting...");

    let config: Config = load_config("macd_grid_config.json")?;

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: None,
            okx_credentials: Some(config.okx),
            hyperliquid_credentials: None,
            ibkr_credentials: None,
        },
        mailbox::unbounded(),
    );

    let strategy = MacdGridStrategy::new(config.strategy);

    manager
        .ask(AddStrategies(vec![Box::new(strategy)]))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!("MACD Grid strategy added");

    wait_for_shutdown(manager).await;
    Ok(())
}

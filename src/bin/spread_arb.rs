use fee_arb::engine::{
    init_monitoring, init_spread_arb_stats, init_tracing, load_config, wait_for_shutdown,
    AddStrategies, DatabaseConfig, ManagerActor, ManagerActorArgs, MonitoringConfig,
};
use fee_arb::exchange::hyperliquid::HyperliquidCredentials;
use fee_arb::exchange::ibkr::IbkrCredentials;
use fee_arb::strategy::{SpreadArbConfig, SpreadArbStrategy};
use kameo::actor::Spawn;
use kameo::mailbox;
use serde::Deserialize;

/// SpreadArb 独立配置
#[derive(Debug, Clone, Deserialize)]
struct SpreadArbAppConfig {
    ibkr: IbkrCredentials,
    hyperliquid: HyperliquidCredentials,
    strategy: SpreadArbConfig,
    monitoring: Option<MonitoringConfig>,
    database: Option<DatabaseConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    tracing::info!("SpreadArb system starting...");

    let config: SpreadArbAppConfig = load_config("spread_arb_config.json")?;

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: None,
            okx_credentials: None,
            hyperliquid_credentials: Some(config.hyperliquid.clone()),
            ibkr_credentials: Some(config.ibkr.clone()),
        },
        mailbox::unbounded(),
    );

    // 为每个 symbol 创建 SpreadArbStrategy
    let hl_dex = &config.hyperliquid.dex;
    let strategies: Vec<Box<dyn fee_arb::strategy::Strategy>> = config
        .strategy
        .symbols
        .iter()
        .map(|symbol| {
            Box::new(SpreadArbStrategy::new(
                config.strategy.clone(),
                symbol.clone(),
                hl_dex,
            )) as Box<dyn fee_arb::strategy::Strategy>
        })
        .collect();

    let strategy_count = strategies.len();

    manager
        .ask(AddStrategies(strategies))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!(count = strategy_count, "SpreadArb strategies added");

    // 监控
    if let Some(ref monitoring) = config.monitoring {
        init_monitoring(&manager, monitoring).await?;
    }

    // SpreadArb 统计 + DB 持久化
    if let Some(ref db_config) = config.database {
        // symbols 需包含 IBKR 侧和 HL 侧 (e.g., "AAPL" + "xyz:AAPL")
        let all_symbols = config.strategy.symbols.iter().flat_map(|s| {
            let hl_s = if hl_dex.is_empty() {
                s.clone()
            } else {
                format!("{}:{}", hl_dex, s)
            };
            vec![s.clone(), hl_s]
        });
        init_spread_arb_stats(&manager, all_symbols, db_config)
            .await?;
    } else {
        tracing::warn!("database is not set, signals/orders/fills will not be persisted");
    }

    wait_for_shutdown(manager).await;
    Ok(())
}

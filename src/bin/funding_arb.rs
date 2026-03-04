use std::collections::{HashMap, HashSet};

use fee_arb::domain::{Exchange, Symbol, SymbolMeta};
use fee_arb::engine::{
    init_monitoring, init_spread_arb_stats, init_tracing, load_config, wait_for_shutdown,
    AddStrategies, DatabaseConfig, GetAllSymbolMetas, ManagerActor, ManagerActorArgs,
    MonitoringConfig,
};
use fee_arb::exchange::binance::BinanceCredentials;
use fee_arb::exchange::hyperliquid::HyperliquidCredentials;
use fee_arb::exchange::ibkr::IbkrCredentials;
use fee_arb::exchange::okx::OkxCredentials;
use fee_arb::strategy::{
    FundingArbConfig, FundingArbStrategy, SpreadArbConfig, SpreadArbStrategy,
};
use kameo::actor::Spawn;
use kameo::mailbox;
use md5::{Digest, Md5};
use serde::Deserialize;

/// 交易所配置
#[derive(Debug, Clone, Deserialize)]
struct ExchangesConfig {
    binance: BinanceCredentials,
    okx: OkxCredentials,
    hyperliquid: HyperliquidCredentials,
    ibkr: Option<IbkrCredentials>,
}

impl ExchangesConfig {
    fn enabled_exchanges(&self) -> Vec<Exchange> {
        let mut exchanges = vec![Exchange::Binance, Exchange::OKX, Exchange::Hyperliquid];
        if self.ibkr.is_some() {
            exchanges.push(Exchange::IBKR);
        }
        exchanges
    }
}

/// 策略配置
#[derive(Debug, Clone, Deserialize)]
struct StrategyConfig {
    funding_arb: FundingArbConfig,
    spread_arb: Option<SpreadArbConfig>,
}

/// 完整配置
#[derive(Debug, Clone, Deserialize)]
struct Config {
    exchanges: ExchangesConfig,
    strategy: StrategyConfig,
    monitoring: Option<MonitoringConfig>,
    database: Option<DatabaseConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    tracing::info!("Fee arbitrage system starting...");

    let config: Config = load_config("config.json")?;

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: Some(config.exchanges.binance.clone()),
            okx_credentials: Some(config.exchanges.okx.clone()),
            hyperliquid_credentials: Some(config.exchanges.hyperliquid.clone()),
            ibkr_credentials: config.exchanges.ibkr.clone(),
        },
        mailbox::unbounded(),
    );

    // 获取所有交易所的 symbol metas
    let metas: HashMap<Exchange, Vec<SymbolMeta>> =
        manager.ask(GetAllSymbolMetas).send().await?;

    // 计算所有交易所 symbol 的并集
    let all_symbols: HashSet<Symbol> = metas
        .values()
        .flat_map(|metas| metas.iter().map(|m| m.symbol.clone()))
        .collect();

    tracing::info!(
        total_symbols = all_symbols.len(),
        "Total unique symbols from all exchanges"
    );

    // 对 symbol 做 MD5 取模 4，选取余数为 0 的（1/4 的 symbol）
    let symbols: Vec<Symbol> = all_symbols
        .into_iter()
        .filter(|symbol| {
            let mut hasher = Md5::new();
            hasher.update(symbol.as_bytes());
            let hash = hasher.finalize();
            let remainder = hash[15] % 4;
            remainder == 0
        })
        .collect();

    if symbols.is_empty() {
        anyhow::bail!("No symbols selected after MD5 filtering");
    }

    tracing::info!(
        selected_symbols = symbols.len(),
        "Symbols selected (MD5 mod 4 == 0)"
    );

    let enabled_exchanges = config.exchanges.enabled_exchanges();

    // 批量创建所有策略
    let mut strategies: Vec<Box<dyn fee_arb::strategy::Strategy>> = symbols
        .iter()
        .map(|symbol| {
            Box::new(FundingArbStrategy::new(
                config.strategy.funding_arb.clone(),
                enabled_exchanges.clone(),
                symbol.clone(),
            )) as Box<dyn fee_arb::strategy::Strategy>
        })
        .collect();

    // SpreadArb 策略 (IBKR 股票 vs Hyperliquid 永续)
    if let Some(ref spread_arb_config) = config.strategy.spread_arb {
        if config.exchanges.ibkr.is_some() {
            for symbol in &spread_arb_config.symbols {
                strategies.push(Box::new(SpreadArbStrategy::new(
                    spread_arb_config.clone(),
                    symbol.clone(),
                )));
            }
            tracing::info!(
                count = spread_arb_config.symbols.len(),
                "SpreadArb strategies created"
            );
        } else {
            tracing::warn!("spread_arb configured but IBKR is not enabled, skipping");
        }
    }

    let strategy_count = strategies.len();

    manager
        .ask(AddStrategies(strategies))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!(count = strategy_count, "Strategies batch added");

    // 监控
    if let Some(ref monitoring) = config.monitoring {
        init_monitoring(&manager, monitoring).await?;
    }

    // SpreadArb 统计 + 持久化
    if let Some(ref spread_arb_config) = config.strategy.spread_arb {
        if config.exchanges.ibkr.is_none() {
            // spread_arb 策略创建时已 warn，此处无需重复
        } else if let Some(ref db_config) = config.database {
            init_spread_arb_stats(&manager, spread_arb_config.symbols.iter().cloned(), db_config)
                .await?;
        } else {
            tracing::warn!("spread_arb configured but database is not set, signals/orders/fills will not be persisted");
        }
    }

    wait_for_shutdown(manager).await;
    Ok(())
}

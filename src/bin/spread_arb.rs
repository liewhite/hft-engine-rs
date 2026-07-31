//! 跨所价差套利 (spread arb) 实盘入口
//!
//! 选币口径：**每个交易所只订阅自己上市的 symbol**（再按 hash 分桶取本实例负责的一份）。
//! 一个 symbol 至少在两个交易所上市才建策略，且策略只拿到真正上市该 symbol 的交易所列表——
//! 避免向交易所订阅它没有的 symbol（会触发 WS 订阅错误 → actor 级联退出）。

use std::collections::{BTreeMap, HashMap};

use hft_engine_rs::domain::{Exchange, Symbol, SymbolMeta};
use hft_engine_rs::engine::{
    init_tracing, load_config, wait_for_shutdown, AddStrategies, GetAllSymbolMetas, ManagerActor,
    ManagerActorArgs, RegisterMetricsSymbols,
};
use hft_engine_rs::exchange::binance::BinanceCredentials;
use hft_engine_rs::exchange::hyperliquid::HyperliquidCredentials;
use hft_engine_rs::exchange::ibkr::IbkrCredentials;
use hft_engine_rs::exchange::okx::OkxCredentials;
use hft_engine_rs::strategy::{SpreadArbConfig, SpreadArbStrategy};
use kameo::actor::Spawn;
use kameo::mailbox;
use md5::{Digest, Md5};
use serde::Deserialize;

/// 一个 symbol 至少要在这么多交易所上市，才有跨所价差可套
const MIN_EXCHANGES_PER_SYMBOL: usize = 2;

/// 交易所凭证配置（缺省即该所不参与）
#[derive(Debug, Clone, Deserialize)]
struct ExchangesConfig {
    binance: Option<BinanceCredentials>,
    okx: Option<OkxCredentials>,
    hyperliquid: Option<HyperliquidCredentials>,
    ibkr: Option<IbkrCredentials>,
}

/// symbol 分桶配置
///
/// 多实例水平拆分时，每个实例只负责 `hash(symbol) % buckets == index` 的那一份。
/// 单实例跑全量则配 `buckets = 1, index = 0`。
#[derive(Debug, Clone, Deserialize)]
struct SymbolBucketConfig {
    /// 分桶总数
    buckets: u64,
    /// 本实例负责的桶序号
    index: u64,
}

impl SymbolBucketConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.buckets == 0 {
            anyhow::bail!("symbol_bucket.buckets 必须 > 0");
        }
        if self.index >= self.buckets {
            anyhow::bail!(
                "symbol_bucket.index ({}) 必须小于 buckets ({})",
                self.index,
                self.buckets
            );
        }
        Ok(())
    }

    /// 该 symbol 是否落在本实例负责的桶里
    ///
    /// 取 MD5 前 8 字节作为 u64（比取单字节取模分布更均匀），对 buckets 取模。
    fn contains(&self, symbol: &Symbol) -> bool {
        let mut hasher = Md5::new();
        hasher.update(symbol.as_bytes());
        let digest = hasher.finalize();
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        u64::from_le_bytes(head) % self.buckets == self.index
    }
}

/// 策略配置
#[derive(Debug, Clone, Deserialize)]
struct StrategyConfig {
    spread_arb: SpreadArbConfig,
    symbol_bucket: SymbolBucketConfig,
}

/// 完整配置
#[derive(Debug, Clone, Deserialize)]
struct Config {
    exchanges: ExchangesConfig,
    strategy: StrategyConfig,
}

/// 从各所 symbol 列表推导「symbol -> 上市该 symbol 的交易所」
///
/// 纯函数：只依赖入参，便于单测。交易所列表排序保证同一输入产生同一结果（启动日志可复现）。
fn build_symbol_universe(
    metas: &HashMap<Exchange, Vec<SymbolMeta>>,
    bucket: &SymbolBucketConfig,
) -> BTreeMap<Symbol, Vec<Exchange>> {
    let mut universe: BTreeMap<Symbol, Vec<Exchange>> = BTreeMap::new();
    for (exchange, exchange_metas) in metas {
        for meta in exchange_metas {
            if !bucket.contains(&meta.symbol) {
                continue;
            }
            universe
                .entry(meta.symbol.clone())
                .or_default()
                .push(*exchange);
        }
    }
    for exchanges in universe.values_mut() {
        exchanges.sort();
        exchanges.dedup();
    }
    universe
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    tracing::info!("Spread arbitrage system starting...");

    let config: Config = load_config("config.json")?;
    config.strategy.symbol_bucket.validate()?;
    config
        .strategy
        .spread_arb
        .validate()
        .map_err(|e| anyhow::anyhow!("strategy.spread_arb 配置非法: {e}"))?;

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance_credentials: config.exchanges.binance.clone(),
            okx_credentials: config.exchanges.okx.clone(),
            hyperliquid_credentials: config.exchanges.hyperliquid.clone(),
            ibkr_credentials: config.exchanges.ibkr.clone(),
            ibkr_snapshot: None,
        },
        mailbox::unbounded(),
    );

    // 等待 Manager 启动完成；启动失败 → 带真实 ExchangeError 上下文受控退出（非零退出码）
    manager
        .wait_for_startup_result()
        .await
        .map_err(|e| anyhow::anyhow!("Manager startup failed: {e}"))?;

    // 各交易所的 symbol metas（只含配了凭证的所——这就是"参与交易所"的单一数据源）
    let metas: HashMap<Exchange, Vec<SymbolMeta>> = manager.ask(GetAllSymbolMetas).send().await?;
    for (exchange, exchange_metas) in &metas {
        tracing::info!(%exchange, listed = exchange_metas.len(), "Exchange symbols listed");
    }

    let universe = build_symbol_universe(&metas, &config.strategy.symbol_bucket);
    tracing::info!(
        buckets = config.strategy.symbol_bucket.buckets,
        index = config.strategy.symbol_bucket.index,
        in_bucket = universe.len(),
        "Symbols selected into this bucket"
    );

    // 只保留至少在两个交易所上市的 symbol——单所 symbol 无跨所价差可套，
    // 建了也永远不会产生信号，只是白占订阅与 executor。
    let (tradable, single_exchange): (Vec<_>, Vec<_>) = universe
        .into_iter()
        .partition(|(_, exchanges)| exchanges.len() >= MIN_EXCHANGES_PER_SYMBOL);

    if !single_exchange.is_empty() {
        tracing::info!(
            dropped = single_exchange.len(),
            "Symbols dropped: listed on fewer than 2 exchanges"
        );
    }

    if tradable.is_empty() {
        anyhow::bail!(
            "No tradable symbol: 本桶内没有任何 symbol 同时在 >= {} 个交易所上市",
            MIN_EXCHANGES_PER_SYMBOL
        );
    }

    let symbols: Vec<Symbol> = tradable.iter().map(|(symbol, _)| symbol.clone()).collect();

    let strategies: Vec<Box<dyn hft_engine_rs::strategy::Strategy>> = tradable
        .into_iter()
        .map(|(symbol, exchanges)| {
            tracing::debug!(%symbol, ?exchanges, "Creating strategy");
            Box::new(SpreadArbStrategy::new(
                config.strategy.spread_arb.clone(),
                exchanges,
                symbol,
            )) as Box<dyn hft_engine_rs::strategy::Strategy>
        })
        .collect();

    let strategy_count = strategies.len();

    // 指标 actor 需要知道跟踪哪些 symbol（与策略集合同源）
    manager
        .ask(RegisterMetricsSymbols(symbols))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to register metrics symbols: {e}"))?;

    manager
        .ask(AddStrategies(strategies))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Actor error: {}", e))?;

    tracing::info!(count = strategy_count, "Strategies batch added");

    wait_for_shutdown(manager).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_engine_rs::exchange::utils::StepFormatter;
    use std::sync::Arc;

    fn meta(exchange: Exchange, symbol: &str) -> SymbolMeta {
        SymbolMeta {
            exchange,
            symbol: symbol.to_string(),
            price_formatter: Arc::new(StepFormatter::new(0.01)),
            size_step: 0.001,
            min_order_size: 0.001,
            contract_size: 1.0,
        }
    }

    fn full_bucket() -> SymbolBucketConfig {
        SymbolBucketConfig {
            buckets: 1,
            index: 0,
        }
    }

    #[test]
    fn universe_keeps_only_exchanges_that_list_the_symbol() {
        let metas = HashMap::from([
            (
                Exchange::Binance,
                vec![
                    meta(Exchange::Binance, "BTC"),
                    meta(Exchange::Binance, "PEPE"),
                ],
            ),
            (Exchange::OKX, vec![meta(Exchange::OKX, "BTC")]),
        ]);

        let universe = build_symbol_universe(&metas, &full_bucket());

        assert_eq!(
            universe.get("BTC"),
            Some(&vec![Exchange::Binance, Exchange::OKX])
        );
        // PEPE 只在 Binance 上市 → 交易所列表里不会出现 OKX
        assert_eq!(universe.get("PEPE"), Some(&vec![Exchange::Binance]));
    }

    #[test]
    fn bucket_partitions_symbols_without_overlap_or_loss() {
        let symbols: Vec<Symbol> = (0..200).map(|i| format!("COIN{i}")).collect();
        let buckets = 4;

        let mut covered = 0;
        for index in 0..buckets {
            let bucket = SymbolBucketConfig { buckets, index };
            covered += symbols.iter().filter(|s| bucket.contains(s)).count();
        }

        // 每个 symbol 恰好属于一个桶：总覆盖数等于 symbol 总数
        assert_eq!(covered, symbols.len());
    }

    #[test]
    fn bucket_selection_is_stable() {
        let bucket = SymbolBucketConfig {
            buckets: 8,
            index: 3,
        };
        let symbol = "BTC".to_string();
        assert_eq!(bucket.contains(&symbol), bucket.contains(&symbol));
    }

    #[test]
    fn bucket_config_rejects_invalid_values() {
        assert!(SymbolBucketConfig {
            buckets: 0,
            index: 0
        }
        .validate()
        .is_err());
        assert!(SymbolBucketConfig {
            buckets: 4,
            index: 4
        }
        .validate()
        .is_err());
        assert!(SymbolBucketConfig {
            buckets: 4,
            index: 3
        }
        .validate()
        .is_ok());
    }
}

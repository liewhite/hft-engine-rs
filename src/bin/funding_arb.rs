//! 币安 × Hyperliquid 资金费套利入口（#668）。
//!
//! 用法：`funding_arb [config.json]`。一个进程接一个 HL 市场（`exchanges.hyperliquid.dex`：
//! "" = 加密币主站，"xyz" = 股票/商品），币和股票各跑一个进程，共用币安账户。
//!
//! 资金费数据：本进程起一个轮询任务，每 `funding_poll_secs` 秒为每个币拉两所过去 24h
//! 已结算的公共费率，整窗作为 `SettledFundingWindow` 自定义事件投递给对应策略。

use hft_engine_rs::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use hft_engine_rs::engine::{
    init_tracing, load_config, setup_binance, setup_hyperliquid, wait_for_shutdown, AddStrategies,
    GetAllSymbolMetas, ManagerActor, ManagerActorArgs, StrategySpec,
};
use hft_engine_rs::exchange::binance::{BinanceClient, BinanceCredentials};
use hft_engine_rs::exchange::hyperliquid::{HyperliquidClient, HyperliquidCredentials};
use hft_engine_rs::exchange::ExchangeAccess;
use hft_engine_rs::engine::live::PublishCustomEvent;
use hft_engine_rs::messaging::CustomEvent;
use hft_engine_rs::strategy::{FundingArbConfig, FundingArbStrategy, SettledFundingWindow, Strategy, FUNDING_WINDOW_MS};
use kameo::actor::{ActorRef, Spawn};
use kameo::mailbox;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangesConfig {
    binance: ExchangeAccess<BinanceCredentials>,
    hyperliquid: ExchangeAccess<HyperliquidCredentials>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    exchanges: ExchangesConfig,
    strategy: FundingArbConfig,
    /// 资金费轮询间隔（秒）。费率只在结算时变，HL 每小时一次，几分钟一轮足够
    funding_poll_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    let path = std::env::args().nth(1).unwrap_or_else(|| "config.json".to_string());
    let config: Config = load_config(&path)?;
    config.strategy.validate().map_err(|e| anyhow::anyhow!("strategy 配置非法: {e}"))?;
    anyhow::ensure!(config.funding_poll_secs > 0, "funding_poll_secs 必须 > 0");
    tracing::info!(dex = %config.exchanges.hyperliquid.dex, pairs = config.strategy.pairs.len(), "funding_arb starting");

    let bn_access = config.exchanges.binance.clone();
    let hl_access = config.exchanges.hyperliquid.clone();
    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            exchanges: vec![setup_binance(bn_access.clone())?, setup_hyperliquid(hl_access.clone())?],
            // 只跑实盘（用户定：不做模拟盘），柜台配置用默认值
            paper: Default::default(),
        },
        mailbox::unbounded(),
    );
    manager
        .wait_for_startup_result()
        .await
        .map_err(|e| anyhow::anyhow!("Manager startup failed: {e}"))?;

    // 配置里的币必须两所都有（HL 侧只含全仓品种）；缺了是配置错，拒绝启动
    let metas: HashMap<Exchange, Vec<SymbolMeta>> = manager.ask(GetAllSymbolMetas).send().await?;
    let listed = |ex: Exchange, s: &Symbol| metas.get(&ex).is_some_and(|v| v.iter().any(|m| &m.symbol == s));
    let missing: Vec<String> = config
        .strategy
        .pairs
        .iter()
        .filter(|p| !listed(Exchange::Binance, &p.symbol) || !listed(Exchange::Hyperliquid, &p.symbol))
        .map(|p| p.symbol.clone())
        .collect();
    anyhow::ensure!(missing.is_empty(), "这些币不在两所（HL 只含全仓品种）同时上市: {missing:?}");

    let params = config.strategy.params();
    let symbols: Vec<Symbol> = params.iter().map(|p| p.symbol.clone()).collect();
    let specs = params
        .into_iter()
        .map(|p| StrategySpec::live(Box::new(FundingArbStrategy::new(p)) as Box<dyn Strategy>))
        .collect();
    manager
        .ask(AddStrategies(specs))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("AddStrategies failed: {e}"))?;
    tracing::info!(count = symbols.len(), "strategies added");

    // 公共接口，不带凭证
    let bn = BinanceClient::new(bn_access.quote.clone(), None)?;
    let hl = HyperliquidClient::new(hl_access.quote.clone(), hl_access.dex.clone(), None)?;
    let poll_manager = manager.clone();
    let interval = Duration::from_secs(config.funding_poll_secs);
    tokio::spawn(async move {
        loop {
            poll_funding(&bn, &hl, &symbols, &poll_manager).await;
            tokio::time::sleep(interval).await;
        }
    });

    wait_for_shutdown(manager).await
}

/// 拉一轮两所过去 24h 已结算费率并投递。单个失败只记 warn：策略侧按数据年龄判"未知"，
/// 持续失败会表现为该币不开不平（日志里能看到原因）。
async fn poll_funding(bn: &BinanceClient, hl: &HyperliquidClient, symbols: &[Symbol], manager: &ActorRef<ManagerActor>) {
    for symbol in symbols {
        for ex in [Exchange::Binance, Exchange::Hyperliquid] {
            let start = now_ms().saturating_sub(FUNDING_WINDOW_MS);
            let got = match ex {
                Exchange::Binance => bn.fetch_settled_funding(symbol, start).await,
                _ => hl.fetch_settled_funding(symbol, start).await,
            };
            match got {
                Ok(entries) => {
                    let w = SettledFundingWindow {
                        exchange: ex,
                        symbol: symbol.clone(),
                        window_start: start,
                        fetched_at: now_ms(),
                        entries,
                    };
                    let ev = CustomEvent::for_symbol(ex, symbol.clone(), w);
                    if let Err(e) = manager.tell(PublishCustomEvent(ev)).send().await {
                        tracing::error!(error = %e, "投递资金费窗口失败");
                    }
                }
                Err(e) => tracing::warn!(%symbol, %ex, error = %e, "拉取已结算资金费失败"),
            }
            // 币安 fundingRate 500 次 / 5 分钟 / IP，放慢一点
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

//! 币安 × Hyperliquid 资金费套利入口（#668）。
//!
//! 用法：`funding_arb [config.json]`。一个进程接一个 HL 市场（`exchanges.hyperliquid.dex`：
//! "" = 加密币主站，"xyz" = 股票/商品），币和股票各跑一个进程，共用币安账户。
//!
//! 资金费数据：本进程起一个轮询任务，每 `funding_poll_secs` 秒为每个币拉两所过去 24h
//! 已结算的公共费率，整窗作为 `SettledFundingWindow` 自定义事件投递给对应策略。

use hft_engine_rs::domain::{now_ms, Exchange, Symbol};
use hft_engine_rs::engine::{
    init_tracing, load_config, setup_binance, setup_hyperliquid, wait_for_shutdown, AddStrategies,
    ManagerActor, ManagerActorArgs, StrategySpec,
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

    // 配置里的币两所是否都上市，由框架投产时校验（缺 SymbolMeta 即拒绝启动）
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
        let mut failures: HashMap<(Symbol, Exchange), u32> = HashMap::new();
        loop {
            poll_funding(&bn, &hl, &symbols, &poll_manager, interval, &mut failures).await;
        }
    });

    wait_for_shutdown(manager).await
}

/// 拉一轮两所过去 24h 已结算费率并投递，请求均匀摊在 `interval` 里（HL info 按 IP 每分钟
/// 1200 权重，fundingHistory 一次约 20；币安 fundingRate 500 次 / 5 分钟）。
/// 偶发失败记 warn（网络会间歇掉线，下一轮就补上）；同一项连续失败 3 轮记 error ——
/// 此时策略侧数据会过期变"未知"，该币不开也不平。
async fn poll_funding(
    bn: &BinanceClient,
    hl: &HyperliquidClient,
    symbols: &[Symbol],
    manager: &ActorRef<ManagerActor>,
    interval: Duration,
    failures: &mut HashMap<(Symbol, Exchange), u32>,
) {
    let gap = interval / (2 * symbols.len().max(1) as u32);
    for symbol in symbols {
        for ex in [Exchange::Binance, Exchange::Hyperliquid] {
            let start = now_ms().saturating_sub(FUNDING_WINDOW_MS);
            let got = match ex {
                Exchange::Binance => bn.fetch_settled_funding(symbol, start).await,
                _ => hl.fetch_settled_funding(symbol, start).await,
            };
            match got {
                Ok(entries) => {
                    failures.remove(&(symbol.clone(), ex));
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
                Err(e) => {
                    let n = failures.entry((symbol.clone(), ex)).or_default();
                    *n += 1;
                    if *n >= 3 {
                        tracing::error!(%symbol, %ex, rounds = *n, error = %e, "已结算资金费连续拉取失败，该币数据将过期、不开不平");
                    } else {
                        tracing::warn!(%symbol, %ex, error = %e, "拉取已结算资金费失败");
                    }
                }
            }
            tokio::time::sleep(gap).await;
        }
    }
}

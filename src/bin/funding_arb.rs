//! 多所资金费套利入口（#668 两所版，#673 扩到币安 / Hyperliquid / OKX）。
//!
//! 用法：`funding_arb [config.json]`。`exchanges` 里配了哪几所就接哪几所（至少两所）；
//! 每个币在哪几所做，由启动时各所的品种目录决定（该币上市的所，至少两所，否则拒绝启动）。
//! HL 侧按 `exchanges.hyperliquid.dexes` 接入多个市场（"" = 加密币主站，"xyz" = 股票/商品），
//! HL 账户须为统一账户（多 dex 共用一份 USDC 保证金），否则启动报错。
//!
//! 配置示例（凭证略）：
//! ```json
//! {
//!   "exchanges": {
//!     "binance": {"quote": "USDT", "credentials": {"api_key": "…", "secret": "…"}},
//!     "hyperliquid": {"quote": "USDC", "dexes": ["", "xyz"],
//!                     "credentials": {"wallet_address": "0x…", "signer_url": "http://127.0.0.1:8080"}},
//!     "okx": {"quote": "USDT", "credentials": {"api_key": "…", "secret": "…", "passphrase": "…"}}
//!   },
//!   "strategy": { "pairs": [{"symbol": "BTC"}, {"symbol": "TSLA"}], "…": "…" },
//!   "funding_poll_secs": 300
//! }
//! ```
//!
//! 资金费数据：本进程起一个轮询任务，每 `funding_poll_secs` 秒为每个币拉它所在各所过去 24h
//! 已结算的公共费率，整窗作为 `SettledFundingWindow` 自定义事件投递给对应策略。

use hft_engine_rs::domain::{now_ms, Exchange, Symbol};
use hft_engine_rs::engine::live::{GetAllSymbolMetas, PublishCustomEvent};
use hft_engine_rs::engine::{
    init_tracing, load_config, setup_binance, setup_hyperliquid, setup_okx, wait_for_shutdown, AddStrategies,
    ExchangeSetup, ManagerActor, ManagerActorArgs, StrategySpec,
};
use hft_engine_rs::exchange::binance::{BinanceClient, BinanceCredentials};
use hft_engine_rs::exchange::hyperliquid::{HyperliquidAccess, HyperliquidClient};
use hft_engine_rs::exchange::okx::{OkxClient, OkxCredentials};
use hft_engine_rs::exchange::{ExchangeAccess, SettledFundingSource};
use hft_engine_rs::messaging::CustomEvent;
use hft_engine_rs::strategy::{FundingArbConfig, FundingArbStrategy, SettledFundingWindow, Strategy, FUNDING_WINDOW_MS};
use kameo::actor::{ActorRef, Spawn};
use kameo::mailbox;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// 要接的所：配了就接。加一所 = 这里加一个字段 + `connect` 里加一段
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangesConfig {
    #[serde(default)]
    binance: Option<ExchangeAccess<BinanceCredentials>>,
    #[serde(default)]
    hyperliquid: Option<HyperliquidAccess>,
    #[serde(default)]
    okx: Option<ExchangeAccess<OkxCredentials>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    exchanges: ExchangesConfig,
    strategy: FundingArbConfig,
    /// 资金费轮询间隔（秒）。费率只在结算时变，HL 每小时一次，几分钟一轮足够
    funding_poll_secs: u64,
}

/// 每所一份：引擎装配 + 公共资金费数据源（不带凭证）
struct Connected {
    setups: Vec<ExchangeSetup>,
    funding_sources: Vec<Arc<dyn SettledFundingSource>>,
}

async fn connect(cfg: &ExchangesConfig) -> anyhow::Result<Connected> {
    let mut c = Connected { setups: Vec::new(), funding_sources: Vec::new() };
    if let Some(a) = &cfg.binance {
        c.setups.push(setup_binance(a.clone())?);
        c.funding_sources.push(Arc::new(BinanceClient::new(a.quote.clone(), None)?));
    }
    if let Some(a) = &cfg.hyperliquid {
        c.setups.push(setup_hyperliquid(a.clone()).await?);
        c.funding_sources.push(Arc::new(HyperliquidClient::connect(a.quote.clone(), a.dexes.clone(), None).await?));
    }
    if let Some(a) = &cfg.okx {
        c.setups.push(setup_okx(a.clone())?);
        c.funding_sources.push(Arc::new(OkxClient::new(a.quote.clone(), None)?));
    }
    anyhow::ensure!(c.setups.len() >= 2, "exchanges 至少要配两所，当前 {}", c.setups.len());
    Ok(c)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    let path = std::env::args().nth(1).unwrap_or_else(|| "config.json".to_string());
    let config: Config = load_config(&path)?;
    config.strategy.validate().map_err(|e| anyhow::anyhow!("strategy 配置非法: {e}"))?;
    anyhow::ensure!(config.funding_poll_secs > 0, "funding_poll_secs 必须 > 0");

    let Connected { setups, funding_sources } = connect(&config.exchanges).await?;
    let exchanges: Vec<Exchange> = funding_sources.iter().map(|s| s.exchange()).collect();
    tracing::info!(?exchanges, pairs = config.strategy.pairs.len(), "funding_arb starting");

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            exchanges: setups,
            // 只跑实盘（用户定：不做模拟盘），柜台配置用默认值
            paper: Default::default(),
        },
        mailbox::unbounded(),
    );
    manager
        .wait_for_startup_result()
        .await
        .map_err(|e| anyhow::anyhow!("Manager startup failed: {e}"))?;

    // 每个币在哪几所做：该币在各所品种目录里出现的所（按配置的所顺序），不足两所拒绝启动
    let metas = manager
        .ask(GetAllSymbolMetas)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GetAllSymbolMetas failed: {e}"))?;
    let listed: HashMap<Exchange, HashSet<Symbol>> = metas
        .into_iter()
        .map(|(ex, ms)| (ex, ms.into_iter().map(|m| m.symbol).collect()))
        .collect();
    let mut coverage: Vec<(Symbol, Vec<Exchange>)> = Vec::new();
    let mut lonely: Vec<String> = Vec::new();
    for p in config.strategy.params() {
        let on: Vec<Exchange> = exchanges
            .iter()
            .copied()
            .filter(|ex| listed.get(ex).is_some_and(|s| s.contains(&p.symbol)))
            .collect();
        if on.len() < 2 {
            lonely.push(format!("{}（只在 {on:?}）", p.symbol));
        }
        coverage.push((p.symbol.clone(), on));
    }
    anyhow::ensure!(lonely.is_empty(), "这些币上市的所不足两个，没法对冲：{}", lonely.join("、"));
    for (symbol, on) in &coverage {
        tracing::info!(%symbol, exchanges = ?on, "币种覆盖");
    }

    let specs = config
        .strategy
        .params()
        .into_iter()
        .zip(coverage.iter())
        .map(|(p, (_, on))| StrategySpec::live(Box::new(FundingArbStrategy::new(p, on.clone())) as Box<dyn Strategy>))
        .collect();
    manager
        .ask(AddStrategies(specs))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("AddStrategies failed: {e}"))?;
    tracing::info!(count = coverage.len(), "strategies added");

    let poll_manager = manager.clone();
    let interval = Duration::from_secs(config.funding_poll_secs);
    tokio::spawn(async move {
        let mut failures: HashMap<(Symbol, Exchange), u32> = HashMap::new();
        loop {
            poll_funding(&funding_sources, &coverage, &poll_manager, interval, &mut failures).await;
        }
    });

    wait_for_shutdown(manager).await
}

/// 拉一轮各币所在各所过去 24h 已结算费率并投递，请求均匀摊在 `interval` 里（HL info 按 IP 每分钟
/// 1200 权重，fundingHistory 一次约 20；币安 fundingRate 500 次 / 5 分钟；OKX 10 次 / 2 秒）。
/// 偶发失败记 warn（网络会间歇掉线，下一轮就补上）；同一项连续失败 3 轮记 error ——
/// 此时策略侧数据会过期变"未知"，该币不开仓、只平两所数据都在的仓位对。
async fn poll_funding(
    sources: &[Arc<dyn SettledFundingSource>],
    coverage: &[(Symbol, Vec<Exchange>)],
    manager: &ActorRef<ManagerActor>,
    interval: Duration,
    failures: &mut HashMap<(Symbol, Exchange), u32>,
) {
    let requests: usize = coverage.iter().map(|(_, on)| on.len()).sum();
    let gap = interval / requests.max(1) as u32;
    for (symbol, on) in coverage {
        for source in sources.iter().filter(|s| on.contains(&s.exchange())) {
            let ex = source.exchange();
            let start = now_ms().saturating_sub(FUNDING_WINDOW_MS);
            match source.fetch_settled_funding(symbol, start).await {
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
                        tracing::error!(%symbol, %ex, rounds = *n, error = %e, "已结算资金费连续拉取失败，该所数据将过期");
                    } else {
                        tracing::warn!(%symbol, %ex, error = %e, "拉取已结算资金费失败");
                    }
                }
            }
            tokio::time::sleep(gap).await;
        }
    }
}

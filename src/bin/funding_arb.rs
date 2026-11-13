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
//!                     "credentials": {"wallet_address": "0x…", "signer_url": "http://127.0.0.1:8080",
//!                                     "market_order_slippage": 0.05}},
//!     "okx": {"quote": "USDT", "credentials": {"api_key": "…", "secret": "…", "passphrase": "…"}}
//!   },
//!   "strategy": { "pairs": [{"symbol": "BTC"}, {"symbol": "TSLA"}], "…": "…" },
//!   "funding_poll_secs": 300
//! }
//! ```
//!
//! 资金费数据：本进程起一个轮询任务，每 `funding_poll_secs` 秒为每个币拉它所在各所过去 24h
//! 已结算的公共费率，整窗作为 `SettledFundingWindow` 自定义事件投递给对应策略。

use hft_engine_rs::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use hft_engine_rs::engine::{
    load_config, run_engine, run_main, setup_all, ExchangesConfig, ManagerArgs, ManagerHandle, StrategySpec,
};
use hft_engine_rs::exchange::SettledFundingSource;
use hft_engine_rs::messaging::CustomEvent;
use hft_engine_rs::strategy::{FundingArbConfig, FundingArbStrategy, SettledFundingWindow, Strategy, FUNDING_WINDOW_MS};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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
    run_main(run).await
}

async fn run() -> anyhow::Result<()> {
    let config: Config = load_config("config.json")?;
    config.strategy.validate().map_err(|e| anyhow::anyhow!("strategy 配置非法: {e}"))?;
    anyhow::ensure!(config.funding_poll_secs > 0, "funding_poll_secs 必须 > 0");
    // 每项数据一轮刷新一次：留出一整轮失败的余量（两轮 ≤ 最大可用年龄），否则数据会周期性过期，
    // 策略在「未知」里反复进出
    anyhow::ensure!(
        config.funding_poll_secs.saturating_mul(2000) <= config.strategy.max_funding_age_ms,
        "funding_poll_secs（{}s）的两倍不得超过 strategy.max_funding_age_ms（{}ms）",
        config.funding_poll_secs,
        config.strategy.max_funding_age_ms
    );

    let setups = setup_all(&config.exchanges).await?;
    anyhow::ensure!(setups.len() >= 2, "exchanges 至少要配两所，当前 {}", setups.len());
    tracing::info!(exchanges = setups.len(), pairs = config.strategy.pairs.len(), "funding_arb starting");

    let args = ManagerArgs {
        exchanges: setups,
        // 只跑实盘（用户定：不做模拟盘），不起模拟柜台
        paper: None,
    };
    // 停机信号 → Ok；manager 意外终止或装配失败 → Err（装配失败也先停机），进程带着原因非零退出
    run_engine(args, |manager| async move {
        // 每个币在哪几所做：该币在各所品种目录里出现的所（按配置的所顺序），不足两所拒绝启动
        let exchanges = manager.exchanges();
        // 各所的公共资金费来源：用引擎装配时建好的 client，不另建
        let funding_sources: Vec<Arc<dyn SettledFundingSource>> = exchanges
            .iter()
            .map(|&ex| manager.settled_funding(ex).ok_or_else(|| anyhow::anyhow!("{ex} 不提供已结算资金费")))
            .collect::<anyhow::Result<_>>()?;
        let mut specs = Vec::new();
        let mut coverage: Vec<(Symbol, Vec<Exchange>)> = Vec::new();
        let mut lonely: Vec<String> = Vec::new();
        for p in config.strategy.params() {
            let on: Vec<SymbolMeta> = exchanges.iter().filter_map(|&ex| manager.symbol_meta(ex, &p.symbol)).collect();
            let on_ex: Vec<Exchange> = on.iter().map(|m| m.exchange).collect();
            tracing::info!(symbol = %p.symbol, exchanges = ?on_ex, "币种覆盖");
            if on.len() < 2 {
                lonely.push(format!("{}（只在 {on_ex:?}）", p.symbol));
                continue;
            }
            coverage.push((p.symbol.clone(), on_ex));
            specs.push(StrategySpec::live(Box::new(FundingArbStrategy::new(p, on)) as Box<dyn Strategy>));
        }
        anyhow::ensure!(lonely.is_empty(), "这些币上市的所不足两个，没法对冲：{}", lonely.join("、"));
        manager
            .add_strategies(specs)
            .await
            .map_err(|e| anyhow::anyhow!("AddStrategies failed: {e}"))?;
        tracing::info!(count = coverage.len(), "strategies added");

        // 轮询器挂在引擎监督树上：它若意外崩溃，整机受控停机并带上死因 —— 裸 spawn 的任务
        // 崩了没人知道，资金费数据随之过期，所有币悄悄变成只平不开
        let requests: Vec<(Symbol, Arc<dyn SettledFundingSource>)> = coverage
            .iter()
            .flat_map(|(symbol, on)| {
                funding_sources
                    .iter()
                    .filter(|s| on.contains(&s.exchange()))
                    .map(move |s| (symbol.clone(), s.clone()))
            })
            .collect();
        let gap = Duration::from_secs(config.funding_poll_secs) / requests.len().max(1) as u32;
        tracing::info!(requests = requests.len(), gap_ms = gap.as_millis() as u64, "资金费轮询器启动");
        let mut poller = FundingPoller { requests, next: 0, failures: HashMap::new(), manager: manager.clone() };
        manager.observers().spawn_fn("FundingPoller", move |scope| async move {
            // 第一轮（含重启后）背靠背拉齐，之后每 gap 拉一项：否则重启后要等一整轮（默认几分钟）
            // 才拿齐各币各所的数据，这段时间策略只平不开。第一轮多快由各所适配层的限流决定，这里不管。
            let mut warmup_left = poller.requests.len();
            loop {
                let wait = if warmup_left > 0 { Duration::ZERO } else { gap };
                tokio::select! {
                    // 停止优先；拉取本身只读，停机时中途放弃无害
                    biased;
                    _ = scope.stopping() => return Ok(()),
                    _ = async { tokio::time::sleep(wait).await; poller.fetch_next().await } => {}
                }
                if warmup_left > 0 {
                    warmup_left -= 1;
                    if warmup_left == 0 {
                        tracing::info!("资金费第一轮已拉完，之后按常规间隔轮询");
                    }
                }
            }
        })?;
        Ok(())
    })
    .await
}

/// 资金费轮询器：第一轮背靠背拉齐，之后每个单项间隔（= 一整轮 `funding_poll_secs` / 项数）拉一项
/// （一个币在一个所过去 24h 的已结算费率），整窗作为自定义事件投递给对应策略。请求均匀摊在一轮里；
/// 各所的限频由适配层的限流器管，这里不按各所限额算间隔。
/// 一次只拉一项；请求慢（断网时每项要等到超时）时下一项顺延，恢复后不连发补拍。
/// 偶发失败记 warn（网络会间歇掉线，下一轮就补上）；同一项连续失败 3 轮记 error —— 此时策略侧
/// 数据会过期变"未知"，该币不开仓、只平两所数据都在的仓位对。
struct FundingPoller {
    /// 每个 (币, 该币上市的所) 一项，轮流拉
    requests: Vec<(Symbol, Arc<dyn SettledFundingSource>)>,
    next: usize,
    failures: HashMap<(Symbol, Exchange), u32>,
    manager: ManagerHandle,
}

impl FundingPoller {
    /// 拉下一项并投递
    async fn fetch_next(&mut self) {
        let Some((symbol, source)) = self.requests.get(self.next).cloned() else { return };
        self.next = (self.next + 1) % self.requests.len();
        let ex = source.exchange();
        let start = now_ms().saturating_sub(FUNDING_WINDOW_MS);
        match source.fetch_settled_funding(&symbol, start).await {
            Ok(entries) => {
                self.failures.remove(&(symbol.clone(), ex));
                let w = SettledFundingWindow {
                    exchange: ex,
                    symbol: symbol.clone(),
                    window_start: start,
                    fetched_at: now_ms(),
                    entries,
                };
                self.manager.publish_custom_event(CustomEvent::for_symbol(ex, symbol, w));
            }
            Err(e) => {
                let n = self.failures.entry((symbol.clone(), ex)).or_default();
                *n += 1;
                if *n >= 3 {
                    tracing::error!(%symbol, %ex, rounds = *n, error = %e, "已结算资金费连续拉取失败，该所数据将过期");
                } else {
                    tracing::warn!(%symbol, %ex, error = %e, "拉取已结算资金费失败");
                }
            }
        }
    }
}

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
use hft_engine_rs::engine::live::{GetAllSymbolMetas, PublishCustomEvent};
use hft_engine_rs::engine::{
    load_config, setup_binance, setup_hyperliquid, setup_okx, run_engine, run_main, spawn_supervised, AddStrategies,
    ExchangeSetup, ManagerActor, ManagerActorArgs, StrategySpec,
};
use hft_engine_rs::exchange::binance::{BinanceClient, BinanceCredentials};
use hft_engine_rs::exchange::hyperliquid::{HyperliquidAccess, HyperliquidClient};
use hft_engine_rs::exchange::okx::{OkxClient, OkxCredentials};
use hft_engine_rs::exchange::{ExchangeAccess, SettledFundingSource};
use hft_engine_rs::messaging::CustomEvent;
use hft_engine_rs::strategy::{FundingArbConfig, FundingArbStrategy, SettledFundingWindow, Strategy, FUNDING_WINDOW_MS};
use kameo::actor::ActorRef;
use kameo::error::Infallible;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_stream::wrappers::IntervalStream;

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
    run_main(run).await
}

async fn run() -> anyhow::Result<()> {
    let config: Config = load_config("config.json")?;
    config.strategy.validate().map_err(|e| anyhow::anyhow!("strategy 配置非法: {e}"))?;
    anyhow::ensure!(config.funding_poll_secs > 0, "funding_poll_secs 必须 > 0");
    // 每项数据一轮刷新一次：轮询一轮不短于最大可用年龄的话，数据会周期性过期，策略在「未知」里反复进出
    anyhow::ensure!(
        config.funding_poll_secs.saturating_mul(1000) < config.strategy.max_funding_age_ms,
        "funding_poll_secs（{}s）必须短于 strategy.max_funding_age_ms（{}ms）",
        config.funding_poll_secs,
        config.strategy.max_funding_age_ms
    );

    let Connected { setups, funding_sources } = connect(&config.exchanges).await?;
    let exchanges: Vec<Exchange> = funding_sources.iter().map(|s| s.exchange()).collect();
    tracing::info!(?exchanges, pairs = config.strategy.pairs.len(), "funding_arb starting");

    let args = ManagerActorArgs {
        exchanges: setups,
        // 只跑实盘（用户定：不做模拟盘），柜台配置用默认值
        paper: Default::default(),
    };
    // 停机信号 → Ok；manager 意外终止或装配失败 → Err（装配失败也先停机），进程带着原因非零退出
    run_engine(args, |manager| async move {
        // 每个币在哪几所做：该币在各所品种目录里出现的所（按配置的所顺序），不足两所拒绝启动
        let metas = manager
            .ask(GetAllSymbolMetas)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GetAllSymbolMetas failed: {e}"))?;
        let listed: HashMap<(Exchange, Symbol), SymbolMeta> = metas
            .into_iter()
            .flat_map(|(ex, ms)| ms.into_iter().map(move |m| ((ex, m.symbol.clone()), m)))
            .collect();
        let mut specs = Vec::new();
        let mut coverage: Vec<(Symbol, Vec<Exchange>)> = Vec::new();
        let mut lonely: Vec<String> = Vec::new();
        for p in config.strategy.params() {
            let on: Vec<SymbolMeta> =
                exchanges.iter().filter_map(|&ex| listed.get(&(ex, p.symbol.clone())).cloned()).collect();
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
            .ask(AddStrategies(specs))
            .send()
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
        spawn_supervised::<FundingPoller>(
            &manager,
            FundingPollerArgs {
                requests,
                round: Duration::from_secs(config.funding_poll_secs),
                manager: manager.clone(),
            },
        )
        .await?;
        Ok(())
    })
    .await
}

struct FundingPollerArgs {
    /// 每个 (币, 该币上市的所) 一项，轮流拉
    requests: Vec<(Symbol, Arc<dyn SettledFundingSource>)>,
    /// 拉完一整轮的时长；单项间隔 = round / 项数
    round: Duration,
    manager: ActorRef<ManagerActor>,
}

/// 资金费轮询器：每个间隔拉一项（一个币在一个所过去 24h 的已结算费率），整窗作为自定义事件
/// 投递给对应策略；请求均匀摊在一轮里（HL info 按 IP 每分钟 1200 权重，fundingHistory 一次约 20；
/// 币安 fundingRate 500 次 / 5 分钟；OKX 10 次 / 2 秒）。一条消息只拉一项，停机时最多等一个请求。
/// 偶发失败记 warn（网络会间歇掉线，下一轮就补上）；同一项连续失败 3 轮记 error —— 此时策略侧
/// 数据会过期变"未知"，该币不开仓、只平两所数据都在的仓位对。
struct FundingPoller {
    requests: Vec<(Symbol, Arc<dyn SettledFundingSource>)>,
    next: usize,
    failures: HashMap<(Symbol, Exchange), u32>,
    manager: ActorRef<ManagerActor>,
}

impl Actor for FundingPoller {
    type Args = FundingPollerArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let gap = args.round / args.requests.len().max(1) as u32;
        let mut ticker = tokio::time::interval(gap);
        // 某次请求慢了就顺延，不在之后连发补拍（会撞各所限频）
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        actor_ref.attach_stream(IntervalStream::new(ticker), (), ());
        tracing::info!(requests = args.requests.len(), gap_ms = gap.as_millis() as u64, "资金费轮询器启动");
        Ok(Self { requests: args.requests, next: 0, failures: HashMap::new(), manager: args.manager })
    }
}

impl FundingPoller {
    async fn poll_one(&mut self) {
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
                let ev = CustomEvent::for_symbol(ex, symbol, w);
                if let Err(e) = self.manager.tell(PublishCustomEvent(ev)).send().await {
                    tracing::error!(error = %e, "投递资金费窗口失败");
                }
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

impl Message<StreamMessage<Instant, (), ()>> for FundingPoller {
    type Reply = ();

    async fn handle(&mut self, msg: StreamMessage<Instant, (), ()>, ctx: &mut Context<Self, Self::Reply>) {
        match msg {
            StreamMessage::Next(_) => self.poll_one().await,
            StreamMessage::Started(_) => {}
            StreamMessage::Finished(_) => {
                // 定时流不会自己结束；真结束了就让监督树停机，不悄悄停止拉数
                tracing::error!("资金费轮询定时流意外结束");
                ctx.actor_ref().kill();
            }
        }
    }
}

//! 无凭证 (non-auth) 公共行情链路端到端验证：BBO + Trades，三个交易所。
//!
//! 与 `exchange::trades_conformance` 的区别：那里只验证 codec 与线路报文一致；这里跑**真实
//! 组件链路** —— 交易所节点 -> public WS 子任务 -> codec -> MarketBus -> 订阅者，
//! 因此还能验证：
//!   1. 三个所在 `credentials: None` 下能否只订阅行情、不碰账户（private WS 是 Option 门控）
//!   2. `SubscriptionKind::Trades` 在真实订阅路径上生效（端点路由 / 频道名 / 事件分发）
//!   3. **数量口径是币本位**：OKX 的 BBO/Trade 量在线路上是张数，必须经 SymbolMeta 折算后
//!      才进 domain。若折算失效，OKX 会因缺 meta 丢弃全部事件（收不到任何数据），或者数量
//!      被放大 contract_size 倍（BTC 为 100 倍）——两者本测试都能发现。
//!
//! 注意 `Manager` **不支持** non-auth：它以 credentials 为门控，None 时不建 client、
//! 不拉 metas、不起交易所节点。故此处直接在任务树上起交易所节点。
//!
//! 运行:
//! ```sh
//! cargo test --test public_stream_noauth -- --ignored --nocapture
//! ```
use hft_engine_rs::messaging::SymbolMarketData;
use rust_decimal_macros::dec;

use hft_engine_rs::domain::{to_f64, Exchange, Symbol, SymbolMeta};
use hft_engine_rs::engine::{AccountBus, MarketBus};
use hft_engine_rs::exchange::binance::{BinanceArgs, BinanceClient, BinanceNode};
use hft_engine_rs::exchange::hyperliquid::{HyperliquidArgs, HyperliquidClient, HyperliquidNode};
use hft_engine_rs::exchange::okx::{OkxArgs, OkxClient, OkxNode};
use hft_engine_rs::exchange::{ExchangeClient, SubscriptionKind};
use hft_engine_rs::messaging::{MarketData, MarketEvent};
use hft_engine_rs::tasks::{Inbox, Root, Scope, Task};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 观测标的（三个所都有的高流动性永续）
const COIN: &str = "BTC";
/// 观测窗口
const OBSERVE: Duration = Duration::from_secs(25);

// ============================================================================
// 事件收集者
// ============================================================================

/// 单个交易所观测到的行情摘要
#[derive(Debug, Default, Clone)]
struct Observed {
    bbo_count: usize,
    trade_count: usize,
    /// 最近一次 BBO 的 (bid_price, bid_qty, ask_qty)
    last_bbo: Option<(f64, f64, f64)>,
    /// 全部成交量样本（币本位）
    trade_qtys: Vec<f64>,
}

type Shared = Arc<Mutex<HashMap<Exchange, Observed>>>;

/// 订阅 MarketBus 并累计各所的 BBO / Trade 观测
struct Collector {
    inbox: Inbox<MarketEvent>,
    shared: Shared,
}

impl Task for Collector {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(ev) = self.inbox.recv().await {
            let mut guard = self.shared.lock().expect("collector mutex");
            match &ev.data {
                MarketData::Symbol(SymbolMarketData::BBO(b)) => {
                    let e = guard.entry(b.exchange).or_default();
                    e.bbo_count += 1;
                    e.last_bbo = Some((to_f64(b.bid_price), to_f64(b.bid_qty), to_f64(b.ask_qty)));
                }
                MarketData::Symbol(SymbolMarketData::MarketTrade(t)) => {
                    let e = guard.entry(t.exchange).or_default();
                    e.trade_count += 1;
                    e.trade_qtys.push(to_f64(t.qty));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// ============================================================================
// 测试主体
// ============================================================================

fn metas_of(metas: Vec<SymbolMeta>) -> Arc<HashMap<Symbol, SymbolMeta>> {
    Arc::new(metas.into_iter().map(|m| (m.symbol.clone(), m)).collect())
}

fn subscriptions() -> Vec<SubscriptionKind> {
    vec![
        SubscriptionKind::BBO {
            symbol: COIN.to_string(),
        },
        SubscriptionKind::Trades {
            symbol: COIN.to_string(),
        },
    ]
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要联网连交易所（无需凭证）"]
async fn noauth_bbo_and_trades_flow_through_node_path() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("hft_engine_rs=warn")
        .try_init();

    // ---- 1. 无凭证 client：symbol meta 走公共 REST，不需要签名 ----
    let binance_client = Arc::new(BinanceClient::new("USDT".to_string()).expect("binance client"));
    let okx_client = Arc::new(OkxClient::new("USDT".to_string()).expect("okx client"));
    let hl_client = Arc::new(
        HyperliquidClient::connect("USDC".to_string(), vec![String::new()])
            .await
            .expect("hl client"),
    );

    let symbol = COIN.to_string();
    let binance_metas = binance_client
        .fetch_all_symbol_metas()
        .await
        .map(|all| {
            all.into_iter()
                .filter(|m| m.symbol == symbol)
                .collect::<Vec<_>>()
        })
        .expect("binance meta (public REST)");
    assert_eq!(binance_metas.len(), 1, "币安品种目录里应有 {symbol}");
    let okx_metas = okx_client
        .fetch_all_symbol_metas()
        .await
        .map(|all| {
            all.into_iter()
                .filter(|m| m.symbol == symbol)
                .collect::<Vec<_>>()
        })
        .expect("okx meta (public REST)");
    // HL 节点直接用 client 的品种目录，这里只确认目录里有该币
    let hl_metas = hl_client
        .fetch_all_symbol_metas()
        .await
        .map(|all| {
            all.into_iter()
                .filter(|m| m.symbol == symbol)
                .collect::<Vec<_>>()
        })
        .expect("hl meta");
    assert_eq!(hl_metas.len(), 1, "HL 品种目录里应有 {symbol}");

    // OKX 的 ctVal 是本次口径验证的关键输入
    let okx_contract_size = okx_metas
        .iter()
        .find(|m| m.symbol == symbol)
        .map(|m| m.contract_size)
        .expect("okx BTC meta");
    println!("OKX BTC contract_size (ctVal) = {okx_contract_size}");
    assert!(
        okx_contract_size > dec!(0.0) && okx_contract_size < dec!(1.0),
        "OKX BTC 每张应小于 1 币，实际 {okx_contract_size}"
    );

    // ---- 2. 总线 + 收集者（消费者组后停：生产者收尾时发的事件照样收得到） ----
    let market_bus = MarketBus::new("market");
    let account_bus = AccountBus::new("account");
    let root = Root::new("test");
    let producers = root.group("producers");
    let consumers = root.group("consumers");
    let shared: Shared = Arc::new(Mutex::new(HashMap::new()));
    {
        let (bus, shared) = (market_bus.clone(), shared.clone());
        consumers
            .spawn("Collector", move |scope: Scope| async move {
                let (_addr, mut inbox) = scope.inbox::<MarketEvent>();
                inbox.subscribe(&bus, |_| true, |e| e);
                Ok(Collector { inbox, shared })
            })
            .await
            .expect("collector");
    }

    // ---- 3. 三个交易所节点，全部 credentials: None ----
    let binance = BinanceNode::spawn(
        &producers,
        BinanceArgs {
            account: None,
            market_bus: market_bus.clone(),
            account_bus: account_bus.clone(),
            quote: "USDT".to_string(),
        },
    )
    .await
    .expect("BinanceNode 应能在无凭证下启动（private WS 由 Option 门控）");
    let okx = OkxNode::spawn(
        &producers,
        OkxArgs {
            account: None,
            symbol_metas: metas_of(okx_metas),
            market_bus: market_bus.clone(),
            account_bus: account_bus.clone(),
            quote: "USDT".to_string(),
        },
    )
    .await
    .expect("OkxNode 应能在无凭证下启动");
    let hl = HyperliquidNode::spawn(
        &producers,
        HyperliquidArgs {
            client: hl_client.clone(),
            account: None,
            market_bus: market_bus.clone(),
            account_bus: account_bus.clone(),
        },
    )
    .await
    .expect("HyperliquidNode 应能在无凭证下启动");
    println!("三个交易所节点均已在 non-auth 模式下启动");

    // ---- 4. 订阅 BBO + Trades ----
    binance
        .subscribe_batch(subscriptions())
        .expect("binance subscribe");
    okx.subscribe_batch(subscriptions()).expect("okx subscribe");
    hl.subscribe_batch(subscriptions()).expect("hl subscribe");

    tokio::time::sleep(OBSERVE).await;

    assert!(!root.is_stopping(), "观测期间有节点出错（见日志）");
    root.shutdown().await.expect("正常停机");
    let observed = shared.lock().expect("mutex").clone();

    // ---- 5. 断言与报告 ----
    for exchange in [Exchange::Binance, Exchange::OKX, Exchange::Hyperliquid] {
        let o = observed
            .get(&exchange)
            .unwrap_or_else(|| panic!("[{exchange}] 观测窗口内没有任何行情事件"));
        let avg_trade = if o.trade_qtys.is_empty() {
            0.0
        } else {
            o.trade_qtys.iter().sum::<f64>() / o.trade_qtys.len() as f64
        };
        println!(
            "[{exchange}] bbo={} trades={} last_bbo={:?} avg_trade_qty={:.6}",
            o.bbo_count, o.trade_count, o.last_bbo, avg_trade
        );

        assert!(o.bbo_count > 0, "[{exchange}] 未收到 BBO");
        assert!(
            o.trade_count > 0,
            "[{exchange}] 未收到 Trades —— 订阅未生效，或 OKX 因缺 SymbolMeta 丢弃了全部成交"
        );
        let (_, bid_qty, ask_qty) = o.last_bbo.expect("last bbo");
        assert!(
            bid_qty > 0.0 && ask_qty > 0.0,
            "[{exchange}] BBO 挂单量必须为正: bid={bid_qty} ask={ask_qty}"
        );
        assert!(
            o.trade_qtys.iter().all(|q| *q > 0.0),
            "[{exchange}] 成交量必须为正"
        );
    }

    // ---- 6. 币本位口径：跨所对比最优价名义额 ----
    //
    // 同一品种、同一时刻，各所最优价的名义额是同一量级；若 OKX 漏了张->币折算，其数量会被
    // 放大 1/ctVal = 100 倍，名义额也随之放大 100 倍。真实的跨所流动性差异远小于此，故用
    // 30 倍作为判据（留足余量，同时能可靠抓住 100 倍的单位错误）。
    const MAX_NOTIONAL_RATIO: f64 = 30.0;
    let notional = |e: Exchange| -> f64 {
        let (price, bid_qty, _) = observed[&e].last_bbo.expect("bbo");
        price * bid_qty
    };
    let binance_notional = notional(Exchange::Binance);
    let okx_notional = notional(Exchange::OKX);
    let ratio = okx_notional / binance_notional;
    println!(
        "最优买价名义额: Binance={binance_notional:.2} USDT, OKX={okx_notional:.2} USDT, ratio={ratio:.2}"
    );
    assert!(
        ratio < MAX_NOTIONAL_RATIO && ratio > 1.0 / MAX_NOTIONAL_RATIO,
        "OKX 最优价名义额与 Binance 相差 {ratio:.1} 倍，疑似张->币折算失效\
         （BTC ctVal={okx_contract_size}，漏折算会放大 {:.0} 倍）",
        1.0 / to_f64(okx_contract_size)
    );

    // OKX 成交量也应是币本位：avg 成交量与 Binance 同量级
    let okx_avg = {
        let q = &observed[&Exchange::OKX].trade_qtys;
        q.iter().sum::<f64>() / q.len() as f64
    };
    let binance_avg = {
        let q = &observed[&Exchange::Binance].trade_qtys;
        q.iter().sum::<f64>() / q.len() as f64
    };
    println!("平均成交量(币): Binance={binance_avg:.6} OKX={okx_avg:.6}");
    assert!(
        okx_avg / binance_avg < MAX_NOTIONAL_RATIO,
        "OKX 平均成交量为 Binance 的 {:.1} 倍，疑似张->币折算失效",
        okx_avg / binance_avg
    );
}

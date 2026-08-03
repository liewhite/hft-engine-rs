//! 无凭证 (non-auth) 公共行情链路端到端验证：BBO + Trades，三个交易所。
//!
//! 与 `exchange::trades_conformance` 的区别：那里只验证 codec 与线路报文一致；这里跑**真实
//! actor 链路** —— 交易所父 actor -> public WS 子 actor -> codec -> IncomePubSub -> 订阅者，
//! 因此还能验证：
//!   1. 三个所在 `credentials: None` 下能否只订阅行情、不碰账户（private WS 是 Option 门控）
//!   2. `SubscriptionKind::Trades` 在真实订阅路径上生效（端点路由 / 频道名 / 事件分发）
//!   3. **数量口径是币本位**：OKX 的 BBO/Trade 量在线路上是张数，必须经 SymbolMeta 折算后
//!      才进 domain。若折算失效，OKX 会因缺 meta 丢弃全部事件（收不到任何数据），或者数量
//!      被放大 contract_size 倍（BTC 为 100 倍）——两者本测试都能发现。
//!
//! 注意 `ManagerActor` **不支持** non-auth：它以 credentials 为门控，None 时不建 client、
//! 不拉 metas、不 spawn 交易所 actor。故此处直接 spawn 交易所 actor。
//!
//! 运行:
//! ```sh
//! cargo test --test public_stream_noauth -- --ignored --nocapture
//! ```

use hft_engine_rs::domain::{Exchange, Symbol, SymbolMeta};
use hft_engine_rs::engine::IncomePubSub;
use hft_engine_rs::exchange::binance::{BinanceActor, BinanceActorArgs, BinanceClient, REST_BASE_URL};
use hft_engine_rs::exchange::hyperliquid::{
    HyperliquidActor, HyperliquidActorArgs, HyperliquidClient,
};
use hft_engine_rs::exchange::okx::{OkxActor, OkxActorArgs, OkxClient};
use hft_engine_rs::exchange::{ExchangeActorOps, ExchangeClient, SubscriptionKind};
use hft_engine_rs::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Subscribe;
use kameo_actors::DeliveryStrategy;
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

/// 订阅 IncomePubSub 并累计各所的 BBO / Trade 观测
struct Collector {
    shared: Shared,
}

impl Actor for Collector {
    type Args = Shared;
    type Error = Infallible;

    async fn on_start(shared: Self::Args, _r: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self { shared })
    }

    async fn on_stop(
        &mut self,
        _r: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl Message<IncomeEvent> for Collector {
    type Reply = ();

    async fn handle(&mut self, ev: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        let mut guard = self.shared.lock().expect("collector mutex");
        match &ev.data {
            ExchangeEventData::BBO(b) => {
                let e = guard.entry(b.exchange).or_default();
                e.bbo_count += 1;
                e.last_bbo = Some((b.bid_price, b.bid_qty, b.ask_qty));
            }
            ExchangeEventData::MarketTrade(t) => {
                let e = guard.entry(t.exchange).or_default();
                e.trade_count += 1;
                e.trade_qtys.push(t.qty);
            }
            _ => {}
        }
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
async fn noauth_bbo_and_trades_flow_through_actor_path() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("hft_engine_rs=warn")
        .try_init();

    // ---- 1. 无凭证 client：symbol meta 走公共 REST，不需要签名 ----
    let binance_client = Arc::new(BinanceClient::new(None).expect("binance client"));
    let okx_client = Arc::new(OkxClient::new(None).expect("okx client"));
    let hl_client = Arc::new(HyperliquidClient::new(None).expect("hl client"));

    let symbol = COIN.to_string();
    let binance_metas = binance_client
        .fetch_symbol_meta(std::slice::from_ref(&symbol))
        .await
        .expect("binance meta (public REST)");
    let okx_metas = okx_client
        .fetch_symbol_meta(std::slice::from_ref(&symbol))
        .await
        .expect("okx meta (public REST)");
    let hl_metas = hl_client
        .fetch_symbol_meta(std::slice::from_ref(&symbol))
        .await
        .expect("hl meta (public REST)");

    // OKX 的 ctVal 是本次口径验证的关键输入
    let okx_contract_size = okx_metas
        .iter()
        .find(|m| m.symbol == symbol)
        .map(|m| m.contract_size)
        .expect("okx BTC meta");
    println!("OKX BTC contract_size (ctVal) = {okx_contract_size}");
    assert!(
        okx_contract_size > 0.0 && okx_contract_size < 1.0,
        "OKX BTC 每张应小于 1 币，实际 {okx_contract_size}"
    );

    // ---- 2. PubSub + 收集者 ----
    let income_pubsub = IncomePubSub::spawn_with_mailbox(
        IncomePubSub::new(DeliveryStrategy::BestEffort),
        mailbox::unbounded(),
    );
    let shared: Shared = Arc::new(Mutex::new(HashMap::new()));
    let collector = Collector::spawn_with_mailbox(shared.clone(), mailbox::unbounded());
    income_pubsub
        .tell(Subscribe(collector.clone()))
        .send()
        .await
        .expect("subscribe collector");

    // ---- 3. 三个交易所 actor，全部 credentials: None ----
    let binance = BinanceActor::spawn_with_mailbox(
        BinanceActorArgs {
            credentials: None,
            symbol_metas: metas_of(binance_metas),
            rest_base_url: REST_BASE_URL.to_string(),
            income_pubsub: income_pubsub.clone(),
            client: binance_client.clone(),
            quote: "USDT".to_string(),
        },
        mailbox::unbounded(),
    );
    let okx = OkxActor::spawn_with_mailbox(
        OkxActorArgs {
            credentials: None,
            client: Some(okx_client.clone()),
            symbol_metas: metas_of(okx_metas),
            income_pubsub: income_pubsub.clone(),
            quote: "USDT".to_string(),
        },
        mailbox::unbounded(),
    );
    let hl = HyperliquidActor::spawn_with_mailbox(
        HyperliquidActorArgs {
            credentials: None,
            symbol_metas: metas_of(hl_metas),
            income_pubsub: income_pubsub.clone(),
            quote: "USDC".to_string(),
            dex: String::new(),
        },
        mailbox::unbounded(),
    );

    binance
        .wait_for_startup_result()
        .await
        .expect("BinanceActor 应能在无凭证下启动（private WS 由 Option 门控）");
    okx.wait_for_startup_result()
        .await
        .expect("OkxActor 应能在无凭证下启动");
    hl.wait_for_startup_result()
        .await
        .expect("HyperliquidActor 应能在无凭证下启动");
    println!("三个交易所 actor 均已在 non-auth 模式下启动");

    // ---- 4. 订阅 BBO + Trades ----
    binance
        .subscribe_batch(subscriptions())
        .await
        .expect("binance subscribe");
    okx.subscribe_batch(subscriptions())
        .await
        .expect("okx subscribe");
    hl.subscribe_batch(subscriptions())
        .await
        .expect("hl subscribe");

    tokio::time::sleep(OBSERVE).await;

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
        1.0 / okx_contract_size
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

//! 模拟盘端到端实测：**真实实盘行情 + 真实策略链路 + 本地柜台**，全程无凭证。
//!
//! 验证"模拟盘只改变 outcome 事件流向"这一设计是否成立 —— 链路与实盘完全相同：
//!
//! ```text
//! 真实公共 WS -> 交易所 actor -> IncomePubSub -> IncomeProcessor -> Executor(StrategyRunner)
//!                                    ^                                      |
//!                                    |                              OutcomePubSub
//!                                    |                                      v
//!                                    +----------- 回报/成交 ------ PaperCounterActor (本地柜台)
//! ```
//!
//! 唯一被替换的是柜台：订单不发往交易所，由本地按"真实成交越过挂单价"判定成交。
//!
//! 策略用一个最小挂单器：拿到 BBO 后在**买价下方若干 bp** 挂 PostOnly 买单，等真实成交打穿。
//! 挂得离盘口越近成交越快，但也越容易在观测窗口内反复成交 —— 这里只要求至少成交一次。
//!
//! 注意：本测试**不经过 ManagerActor**。manager 目前以凭证为交易所开关，模拟盘想完全脱离
//! 凭证运行还需要把"启用哪个所 + 计价币"从凭证里拆出来（见 README/提交说明）。
//!
//! 运行:
//! ```sh
//! cargo test --test paper_trading_live -- --ignored --nocapture
//! ```

use hft_engine_rs::domain::{
    AccountId, Exchange, Order, OrderType, Side, Symbol, SymbolMeta, TimeInForce,
};
use hft_engine_rs::engine::{
    AccountPubSub, IncomeProcessorActor, MarketPubSub, OutcomePubSub, PaperCounterActor,
    PaperCounterArgs, RegisterExecutor,
};
use hft_engine_rs::engine::live::{ExecutorActor, ExecutorArgs};
use hft_engine_rs::exchange::binance::{
    BinanceActor, BinanceActorArgs, BinanceClient, REST_BASE_URL,
};
use hft_engine_rs::exchange::{ExchangeActorOps, ExchangeClient, SubscriptionKind};
use hft_engine_rs::messaging::{AccountData, AccountEvent, IncomeEvent, MarketData, MarketEvent, StateManager};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::{OutcomeEvent, Strategy};
use kameo::actor::{ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Subscribe;
use kameo_actors::DeliveryStrategy;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const EX: Exchange = Exchange::Binance;
/// 本测试的模拟账户
static PAPER_ACCOUNT: std::sync::LazyLock<AccountId> =
    std::sync::LazyLock::new(|| AccountId::Paper("BTC".to_string()));
const COIN: &str = "BTC";
/// 挂单相对买价的下偏幅度（bp）。
///
/// 取 0 即挂在当前买价上：PostOnly 不会吃单（bid < ask 故不可成交，正常入簿），此后**任何
/// 严格低于该价的真实成交**都会触发成交 —— 买价下跳一次即可，比下偏挂单快得多。
/// 撮合是严格越价（不含相等），所以成交在挂单价上打平不算成交。
const OFFSET_BP: f64 = 0.0;
/// 单笔下单量（币）
const ORDER_QTY: f64 = 0.002;
/// 订单到柜台的单程延迟
const ORDER_DELAY_MS: u64 = 50;
/// 观测窗口
const OBSERVE: Duration = Duration::from_secs(60);

// ============================================================================
// 最小挂单策略：空仓则在买价下方挂 PostOnly 买单，有挂单/有仓位则不动
// ============================================================================

/// 最小挂单器：空仓且无挂单时，在当前买价上挂一张 PostOnly 买单，此后不动。
///
/// 挂单价由策略自己写进共享 log：`OrderUpdate` 不带价格（四所里两所是写死的，见
/// `domain::OrderUpdate` 的文档）。下单方本来就知道自己挂在哪，不需要回报捎带。
struct DipMaker(Arc<Mutex<Log>>);

impl Strategy for DipMaker {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        HashMap::from([(
            EX,
            HashSet::from([
                SubscriptionKind::BBO {
                    symbol: COIN.to_string(),
                },
                SubscriptionKind::Trades {
                    symbol: COIN.to_string(),
                },
            ]),
        )])
    }

    fn order_timeout_ms(&self) -> u64 {
        0
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent> {
        let IncomeEvent::Market(MarketEvent {
            data: MarketData::BBO(bbo),
            ..
        }) = event
        else {
            return Vec::new();
        };
        let symbol = COIN.to_string();
        let Some(symbol_state) = state.symbol_state(&symbol) else {
            return Vec::new();
        };
        // 成交过就收手，本测试只需要一笔
        if symbol_state.position_size(EX).abs() > 0.0 {
            return Vec::new();
        }

        if symbol_state.pending_orders().next().is_some() {
            return Vec::new();
        }
        let price = bbo.bid_price * (1.0 - OFFSET_BP / 10_000.0);
        self.0.lock().unwrap().limit_price = Some(price);
        vec![OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: EX,
                symbol: COIN.to_string(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price,
                    tif: TimeInForce::PostOnly,
                },
                quantity: ORDER_QTY,
                reduce_only: false,
                client_order_id: String::new(),
            }],
            comment: format!("paper_dip_buy | bid={} | offset={OFFSET_BP}bp", bbo.bid_price),
        }]
    }
}

// ============================================================================
// 观测者：记录柜台回报
// ============================================================================

#[derive(Default)]
struct Log {
    order_updates: Vec<(String, String)>,
    fills: Vec<(f64, f64)>,
    equity: Option<f64>,
    /// 诊断：柜台侧能看到多少条成交、价格区间走到哪里
    trade_count: usize,
    min_trade_price: Option<f64>,
    max_trade_price: Option<f64>,
    /// 挂单入簿后的限价（取整后）
    /// 策略挂出的价格（由策略自己写入，见 DipMaker）
    limit_price: Option<f64>,
    /// 挂单已被柜台确认入簿（收到 Pending 回报）
    resting: bool,
    /// **挂单入簿之后**的最低成交价 —— 蕴含判断只能用这个，入簿前的成交与该单无关
    min_trade_after_rest: Option<f64>,
}

struct Watcher(Arc<Mutex<Log>>);

impl Actor for Watcher {
    type Args = Arc<Mutex<Log>>;
    type Error = Infallible;
    async fn on_start(a: Self::Args, _r: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self(a))
    }
    async fn on_stop(
        &mut self,
        _r: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// 柜台回报（账户事件，带 Paper 标签）
impl Message<AccountEvent> for Watcher {
    type Reply = ();
    async fn handle(&mut self, ev: AccountEvent, _c: &mut Context<Self, Self::Reply>) {
        let mut log = self.0.lock().unwrap();
        match &ev.data {
            AccountData::OrderUpdate(u) => {
                log.order_updates
                    .push((u.order_id.clone(), format!("{:?}", u.status)));
                if matches!(u.status, hft_engine_rs::domain::OrderStatus::Pending) {
                    log.resting = true;
                }
            }
            AccountData::Fill(f) => log.fills.push((f.price, f.size)),
            AccountData::AccountInfo { equity, .. } => log.equity = Some(*equity),
            _ => {}
        }
    }
}

/// 行情观测（成交价区间诊断）
impl Message<MarketEvent> for Watcher {
    type Reply = ();
    async fn handle(&mut self, ev: MarketEvent, _c: &mut Context<Self, Self::Reply>) {
        let mut log = self.0.lock().unwrap();
        if let MarketData::MarketTrade(t) = &ev.data {
            log.trade_count += 1;
            log.min_trade_price = Some(log.min_trade_price.map_or(t.price, |m: f64| m.min(t.price)));
            log.max_trade_price = Some(log.max_trade_price.map_or(t.price, |m: f64| m.max(t.price)));
            if log.resting {
                log.min_trade_after_rest =
                    Some(log.min_trade_after_rest.map_or(t.price, |m: f64| m.min(t.price)));
            }
        }
    }
}

// ============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要联网连交易所（无需凭证），观测约 60s"]
async fn paper_counter_fills_from_live_trades() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("hft_engine_rs=info")
        .try_init();

    // ---- 无凭证 client + 公共 symbol meta ----
    let client = Arc::new(BinanceClient::new("USDT".to_string(), None).expect("client"));
    let symbol = COIN.to_string();
    let metas = client
        .fetch_symbol_meta(std::slice::from_ref(&symbol))
        .await
        .expect("public symbol meta");
    let metas_by_symbol: Arc<HashMap<Symbol, SymbolMeta>> =
        Arc::new(metas.iter().cloned().map(|m| (m.symbol.clone(), m)).collect());
    let metas_by_key: Arc<HashMap<(Exchange, Symbol), SymbolMeta>> =
        Arc::new(metas.into_iter().map(|m| ((EX, m.symbol.clone()), m)).collect());

    // ---- 总线 ----
    let market_pubsub = MarketPubSub::spawn_with_mailbox(
        MarketPubSub::new(DeliveryStrategy::BestEffort),
        mailbox::unbounded(),
    );
    let account_pubsub = AccountPubSub::spawn_with_mailbox(
        AccountPubSub::new(DeliveryStrategy::BestEffort),
        mailbox::unbounded(),
    );
    let outcome_pubsub = OutcomePubSub::spawn_with_mailbox(
        OutcomePubSub::new(DeliveryStrategy::BestEffort),
        mailbox::unbounded(),
    );

    // ---- 本地柜台：订阅 outcome（收订单）与 income（收行情/Clock）----
    // 模拟账户私有事件走独立总线
    let counter = PaperCounterActor::spawn_with_mailbox(
        PaperCounterArgs {
            account_pubsub: account_pubsub.clone(),
            symbol_metas: metas_by_key.clone(),
            config: SimConfig {
                initial_balance_usdt: 10_000.0,
                // 币安标准档：maker 0.02% / taker 0.05%
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0005,
                order_to_exchange_delay_ms: ORDER_DELAY_MS,
                exchange_to_strategy_delay_ms: 0,
            },
        },
        mailbox::unbounded(),
    );
    outcome_pubsub
        .tell(Subscribe(counter.clone()))
        .send()
        .await
        .expect("counter subscribes outcome");
    market_pubsub
        .tell(Subscribe(counter))
        .send()
        .await
        .expect("counter subscribes income");

    // ---- 策略链路：IncomeProcessor -> Executor ----
    let processor = IncomeProcessorActor::spawn_with_mailbox(
        IncomeProcessorActor::default(),
        mailbox::unbounded(),
    );
    market_pubsub
        .tell(Subscribe(processor.clone()))
        .send()
        .await
        .expect("processor subscribes income");
    account_pubsub
        .tell(Subscribe(processor.clone()))
        .send()
        .await
        .expect("processor subscribes paper");

    // 观测 log 提前创建：策略要把自己的挂单价写进去（回报不再捎带价格）
    let log = Arc::new(Mutex::new(Log::default()));

    let executor = ExecutorActor::spawn_with_mailbox(
        ExecutorArgs {
            strategy: Box::new(DipMaker(Arc::clone(&log))),
            account: PAPER_ACCOUNT.clone(),
            symbol_metas: metas_by_key.clone(),
            // 模拟账户从零起步，无基线
            baselines: Vec::new(),
            outcome_pubsub: outcome_pubsub.clone(),
        },
        mailbox::unbounded(),
    );
    processor
        .tell(RegisterExecutor {
            executor: executor.clone(),
            subscriptions: HashSet::from([(
                EX,
                SubscriptionKind::BBO {
                    symbol: COIN.to_string(),
                },
            )]),
            account: PAPER_ACCOUNT.clone(),
        })
        .send()
        .await
        .expect("register executor");

    // ---- 观测者 ----
    let watcher = Watcher::spawn_with_mailbox(log.clone(), mailbox::unbounded());
    market_pubsub
        .tell(Subscribe(watcher.clone()))
        .send()
        .await
        .expect("watcher subscribes income");
    // 柜台回报走 paper 总线
    account_pubsub
        .tell(Subscribe(watcher))
        .send()
        .await
        .expect("watcher subscribes paper");

    // ---- 交易所 actor：无凭证，只订公共行情 ----
    let binance = BinanceActor::spawn_with_mailbox(
        BinanceActorArgs {
            credentials: None,
            symbol_metas: metas_by_symbol,
            rest_base_url: REST_BASE_URL.to_string(),
            market_pubsub: market_pubsub.clone(),
            account_pubsub: account_pubsub.clone(),
            quote: "USDT".to_string(),
        },
        mailbox::unbounded(),
    );
    binance
        .wait_for_startup_result()
        .await
        .expect("binance actor (non-auth)");
    binance
        .subscribe_batch(vec![
            SubscriptionKind::BBO {
                symbol: COIN.to_string(),
            },
            SubscriptionKind::Trades {
                symbol: COIN.to_string(),
            },
        ])
        .await
        .expect("subscribe");

    // 柜台需要 Clock 才发布净值；这里自己推几拍，不必拉起 ClockActor
    let clock_pubsub = market_pubsub.clone();
    let clock = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let ts = hft_engine_rs::domain::now_ms();
            let _ = clock_pubsub
                .tell(kameo_actors::pubsub::Publish(MarketEvent {
                    exchange_ts: ts,
                    local_ts: ts,
                    data: MarketData::Clock,
                }))
                .send()
                .await;
        }
    });

    tokio::time::sleep(OBSERVE).await;
    clock.abort();

    let log = log.lock().unwrap();
    println!("order updates: {:?}", log.order_updates);
    println!("fills: {:?}", log.fills);
    println!("equity: {:?}", log.equity);
    println!(
        "diag: trades_seen={} price_range=[{:?}, {:?}] limit={:?}",
        log.trade_count, log.min_trade_price, log.max_trade_price, log.limit_price
    );

    assert!(
        log.trade_count > 0,
        "柜台侧未收到任何真实成交 —— trades 流没进到本地柜台，撮合无从发生"
    );

    assert!(
        !log.order_updates.is_empty(),
        "柜台未回报任何订单状态 —— outcome 未流到本地柜台"
    );
    assert!(
        log.order_updates.iter().all(|(id, _)| id.starts_with("paper-")),
        "订单号应由本地柜台发放（paper-*），实际 {:?}",
        log.order_updates
    );
    assert!(
        log.order_updates.iter().any(|(_, s)| s == "Pending"),
        "订单应先进入本地挂单簿（Pending），实际 {:?}",
        log.order_updates
    );
    // 成交与否取决于真实行情是否跌破挂单价 —— 这不是本测试能控制的，故断言**蕴含关系**：
    // 「挂单入簿后出现了严格越价的真实成交」⟹「柜台必须给出成交」。撮合语义本身由
    // engine::live::paper_counter 的确定性单测覆盖，此处只验真实链路。
    let limit = log.limit_price.expect("挂单应已入簿并回报 Pending");
    match log.min_trade_after_rest {
        Some(min_after) if min_after < limit => {
            assert!(
                !log.fills.is_empty(),
                "入簿后最低成交价 {min_after} 已严格跌破挂单价 {limit}，但柜台未给出成交 —— 撮合链路有问题"
            );
            for (price, size) in &log.fills {
                assert!(*price > 0.0);
                assert!(
                    (*size - ORDER_QTY).abs() < 1e-12,
                    "不做部分成交：成交量应等于挂单量 {ORDER_QTY}，实际 {size}"
                );
            }
            println!("✓ 越价成交路径已被真实行情触发并验证");
        }
        other => {
            assert!(
                log.fills.is_empty(),
                "入簿后最低成交价 {other:?} 未跌破挂单价 {limit}，却出现了成交 —— 撮合过于乐观"
            );
            println!(
                "· 观测窗口内行情未跌破挂单价（入簿后最低 {other:?} vs 挂单 {limit}），\
                 已验证「未越价则不成交」这一侧；越价成交由单测覆盖"
            );
        }
    }

    assert!(log.equity.is_some(), "柜台应在 Clock 时发布净值");
}

//! IBKR 集成测试
//!
//! 运行前需要创建 `tests/ibkr_config.json`，格式参见 `ibkr_config.json.example`。
//! 支持 Gateway 和 OAuth 两种模式。
//!
//! 运行: cargo test ibkr -- --ignored

use fee_arb::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce, BBO};
use fee_arb::exchange::ibkr::{IbkrActor, IbkrActorArgs, IbkrClient, IbkrCredentials};
use fee_arb::exchange::{ExchangeClient, SubscribeBatch, SubscriptionKind};
use fee_arb::engine::IncomePubSub;
use fee_arb::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, Spawn};
use kameo::error::Infallible;
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Subscribe as PubSubSubscribe;
use kameo_actors::DeliveryStrategy;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// 测试数量 — 股票最小 1 股
const TEST_QUANTITY: f64 = 1.0;

// ─── BBO 收集器 Actor (用于 WebSocket 测试) ─────────────────

struct BboCollector {
    collected: Arc<Mutex<Vec<BBO>>>,
    notify: Arc<Notify>,
}

impl Actor for BboCollector {
    type Args = (Arc<Mutex<Vec<BBO>>>, Arc<Notify>);
    type Error = Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self {
            collected: args.0,
            notify: args.1,
        })
    }
}

impl Message<IncomeEvent> for BboCollector {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        if let ExchangeEventData::BBO(bbo) = msg.data {
            println!("收到 BBO: {} bid={} ask={}", bbo.symbol, bbo.bid_price, bbo.ask_price);
            self.collected.lock().unwrap().push(bbo);
            self.notify.notify_one();
        }
    }
}

/// 从 tests/ibkr_config.json 读取凭证
fn load_credentials() -> IbkrCredentials {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ibkr_config.json");
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}。请参照 ibkr_config.json.example 创建", path, e));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("解析 ibkr_config.json 失败: {}", e))
}

fn first_symbol(credentials: &IbkrCredentials) -> Symbol {
    credentials.symbols().first().expect("配置中至少需要一个 symbol").clone()
}

// ─── 测试用例 ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_connect() {
    let credentials = load_credentials();
    let symbols = credentials.symbols().to_vec();

    let client = IbkrClient::new(&credentials)
        .await
        .expect("IBKR 连接失败");

    let conids = client.conids();
    println!("连接成功，conids: {:?}", conids);

    assert!(!conids.is_empty(), "应至少解析到一个 conid");
    for sym in &symbols {
        assert!(conids.contains_key(sym), "symbol {} 应有对应 conid", sym);
    }
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_fetch_account_info() {
    let credentials = load_credentials();

    let client = IbkrClient::new(&credentials)
        .await
        .expect("IBKR 连接失败");

    let info = client.fetch_account_info().await.expect("获取账户信息失败");
    println!("账户信息: equity={}, notional={}", info.equity, info.notional);

    assert!(info.equity > 0.0, "equity 应大于 0");
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_symbol_metas() {
    let credentials = load_credentials();
    let symbol = first_symbol(&credentials);

    let client = IbkrClient::new(&credentials)
        .await
        .expect("IBKR 连接失败");

    // fetch_all_symbol_metas
    let all_metas = client.fetch_all_symbol_metas().await.expect("获取全部 SymbolMeta 失败");
    println!("全部 SymbolMeta ({} 个): {:?}", all_metas.len(), all_metas);
    assert!(!all_metas.is_empty(), "应至少有一个 SymbolMeta");

    // fetch_symbol_meta (单个)
    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    assert_eq!(metas.len(), 1, "应返回 1 个 SymbolMeta");
    let meta = &metas[0];
    assert_eq!(meta.symbol, symbol);
    assert_eq!(meta.exchange, Exchange::IBKR);

    // 验证价格格式化 (股票 0.01 步长)
    let formatted = meta.format_price(123.456);
    println!("价格格式化: 123.456 -> {}", formatted);
    assert_eq!(formatted, "123.46");
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_limit_buy() {
    let credentials = load_credentials();
    let symbol = first_symbol(&credentials);

    let client = IbkrClient::new(&credentials)
        .await
        .expect("IBKR 连接失败");

    // 获取 snapshot 中间价
    let mid_price = client
        .fetch_snapshot_mid_price(&symbol)
        .await
        .expect("获取 snapshot 价格失败");
    println!("snapshot 中间价: {}", mid_price);

    // 获取 SymbolMeta 格式化价格
    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    let meta = metas.first().expect("未找到交易对元数据");

    let formatted_price = meta.format_price(mid_price);
    let price: f64 = formatted_price.parse().expect("价格解析失败");
    println!("下单价格: {} (格式化后: {})", mid_price, price);

    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::IBKR,
        symbol,
        side: Side::Long,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::IOC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: uuid::Uuid::new_v4().to_string(),
    };

    println!("提交买单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("买单成功，订单ID: {}", order_id);
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_limit_sell() {
    let credentials = load_credentials();
    let symbol = first_symbol(&credentials);

    let client = IbkrClient::new(&credentials)
        .await
        .expect("IBKR 连接失败");

    // 获取 snapshot 中间价
    let mid_price = client
        .fetch_snapshot_mid_price(&symbol)
        .await
        .expect("获取 snapshot 价格失败");
    println!("snapshot 中间价: {}", mid_price);

    // 获取 SymbolMeta 格式化价格
    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    let meta = metas.first().expect("未找到交易对元数据");

    let formatted_price = meta.format_price(mid_price);
    let price: f64 = formatted_price.parse().expect("价格解析失败");
    println!("下单价格: {} (格式化后: {})", mid_price, price);

    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::IBKR,
        symbol,
        side: Side::Short,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::IOC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: uuid::Uuid::new_v4().to_string(),
    };

    println!("提交卖单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("卖单成功，订单ID: {}", order_id);
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_ws_bbo() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init()
        .ok();

    let credentials = load_credentials();
    let symbols = credentials.symbols().to_vec();

    // 1. 创建 IbkrClient 获取 auth + conids
    let client = Arc::new(
        IbkrClient::new(&credentials)
            .await
            .expect("IBKR 连接失败"),
    );

    let auth = client.auth();
    let conids = client.conids().clone();
    println!("连接成功，conids: {:?}", conids);

    // 2. 创建 IncomePubSub
    let income_pubsub = IncomePubSub::spawn_with_mailbox(
        IncomePubSub::new(DeliveryStrategy::BestEffort),
        mailbox::unbounded(),
    );

    // 3. 创建 BBO 收集器并订阅 PubSub
    let collected = Arc::new(Mutex::new(Vec::<BBO>::new()));
    let notify = Arc::new(Notify::new());
    let collector = BboCollector::spawn_with_mailbox(
        (collected.clone(), notify.clone()),
        mailbox::unbounded(),
    );
    income_pubsub
        .tell(PubSubSubscribe(collector))
        .send()
        .await
        .expect("订阅 IncomePubSub 失败");

    // 4. 启动 IbkrActor (含 WebSocket 连接)
    let ibkr_actor = IbkrActor::spawn_with_mailbox(
        IbkrActorArgs {
            auth,
            income_pubsub,
            conids,
            client,
        },
        mailbox::unbounded(),
    );

    // 5. 订阅所有 symbol 的 BBO
    let bbo_kinds: Vec<SubscriptionKind> = symbols
        .iter()
        .map(|s| SubscriptionKind::BBO { symbol: s.clone() })
        .collect();
    println!("订阅 BBO: {:?}", bbo_kinds);
    ibkr_actor
        .tell(SubscribeBatch { kinds: bbo_kinds })
        .send()
        .await
        .expect("发送 SubscribeBatch 失败");

    // 6. 等待收到 BBO (最多 30 秒)
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            _ = notify.notified() => {
                let count = collected.lock().unwrap().len();
                // 收到足够多的 BBO 就提前结束
                if count >= 3 {
                    break;
                }
            }
            _ = tokio::time::sleep(remaining) => {
                break;
            }
        }
    }

    // 7. 验证
    let bbos = collected.lock().unwrap().clone();
    println!("共收到 {} 条 BBO 数据", bbos.len());
    for bbo in &bbos {
        println!(
            "  {} bid={:.2} ask={:.2} bid_qty={:.0} ask_qty={:.0}",
            bbo.symbol, bbo.bid_price, bbo.ask_price, bbo.bid_qty, bbo.ask_qty
        );
    }

    assert!(!bbos.is_empty(), "应至少收到一条 BBO 数据");
    for bbo in &bbos {
        assert_eq!(bbo.exchange, Exchange::IBKR);
        assert!(bbo.bid_price > 0.0, "bid_price 应大于 0");
        assert!(bbo.ask_price > 0.0, "ask_price 应大于 0");
        assert!(bbo.ask_price >= bbo.bid_price, "ask 应 >= bid");
    }

    // 清理
    ibkr_actor.kill();
}

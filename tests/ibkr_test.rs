//! IBKR 集成测试
//!
//! 运行前需要创建 `tests/ibkr_config.json`，格式参见 `ibkr_config.json.example`。
//! 支持 Gateway 和 OAuth 两种模式。
//!
//! 运行: cargo test ibkr -- --ignored

use fee_arb::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use fee_arb::exchange::ibkr::{IbkrClient, IbkrCredentials};
use fee_arb::exchange::ExchangeClient;

/// 测试数量 — 股票最小 1 股
const TEST_QUANTITY: f64 = 1.0;

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

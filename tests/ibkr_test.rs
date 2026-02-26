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

/// 测试配置（IbkrCredentials + symbols）
struct TestConfig {
    credentials: IbkrCredentials,
}

impl TestConfig {
    fn symbols(&self) -> &[String] {
        self.credentials.symbols()
    }

    fn first_symbol(&self) -> Symbol {
        self.symbols().first().expect("配置中至少需要一个 symbol").clone()
    }
}

/// 从 tests/ibkr_config.json 读取凭证
fn load_config() -> TestConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ibkr_config.json");
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}。请参照 ibkr_config.json.example 创建", path, e));
    let credentials: IbkrCredentials = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("解析 ibkr_config.json 失败: {}", e));
    TestConfig { credentials }
}

/// 通过 snapshot API 获取最新价格
///
/// IBKR 股票无公开 BBO REST API，使用 `/iserver/marketdata/snapshot` 获取。
/// 需要已初始化的 IbkrClient（内含 auth + conids）。
async fn fetch_snapshot_price(client: &IbkrClient, symbol: &Symbol) -> f64 {
    let conid = client.conids().get(symbol)
        .unwrap_or_else(|| panic!("symbol {} 未找到 conid", symbol));

    let http = client.auth().build_http_client().expect("创建 HTTP 客户端失败");
    let url = format!(
        "{}iserver/marketdata/snapshot?conids={}&fields=84,86",
        client.auth().base_url(),
        conid
    );

    // snapshot 可能需要多次请求才能拿到数据（首次请求触发订阅）
    for attempt in 0..3 {
        let resp = client.auth()
            .authed_request(&http, "GET", &url)
            .expect("构建请求失败")
            .send()
            .await
            .expect("snapshot 请求失败");

        let body: serde_json::Value = resp.json().await.expect("解析 snapshot 响应失败");

        if let Some(arr) = body.as_array() {
            if let Some(first) = arr.first() {
                // field 84 = bid, field 86 = ask
                let bid = extract_price(first, "84");
                let ask = extract_price(first, "86");

                if let (Some(b), Some(a)) = (bid, ask) {
                    println!("snapshot attempt {}: bid={}, ask={}", attempt, b, a);
                    return (b + a) / 2.0; // 用中间价作为参考
                }
            }
        }

        println!("snapshot attempt {}: 数据未就绪，等待重试...", attempt);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    panic!("3 次尝试后仍无法获取 {} 的 snapshot 价格", symbol);
}

/// 从 snapshot 响应中提取价格字段
fn extract_price(data: &serde_json::Value, field: &str) -> Option<f64> {
    data.get(field).and_then(|v| {
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

// ─── 测试用例 ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_connect() {
    let config = load_config();

    let client = IbkrClient::new(&config.credentials)
        .await
        .expect("IBKR 连接失败");

    let conids = client.conids();
    println!("连接成功，conids: {:?}", conids);

    assert!(!conids.is_empty(), "应至少解析到一个 conid");
    for sym in config.symbols() {
        assert!(conids.contains_key(sym), "symbol {} 应有对应 conid", sym);
    }
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_fetch_account_info() {
    let config = load_config();

    let client = IbkrClient::new(&config.credentials)
        .await
        .expect("IBKR 连接失败");

    let info = client.fetch_account_info().await.expect("获取账户信息失败");
    println!("账户信息: equity={}, notional={}", info.equity, info.notional);

    assert!(info.equity > 0.0, "equity 应大于 0");
}

#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_ibkr_symbol_metas() {
    let config = load_config();
    let symbol = config.first_symbol();

    let client = IbkrClient::new(&config.credentials)
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
    let config = load_config();
    let symbol = config.first_symbol();

    let client = IbkrClient::new(&config.credentials)
        .await
        .expect("IBKR 连接失败");

    // 获取 snapshot 价格
    let mid_price = fetch_snapshot_price(&client, &symbol).await;
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
    let config = load_config();
    let symbol = config.first_symbol();

    let client = IbkrClient::new(&config.credentials)
        .await
        .expect("IBKR 连接失败");

    // 获取 snapshot 价格
    let mid_price = fetch_snapshot_price(&client, &symbol).await;
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

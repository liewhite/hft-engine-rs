//! Hyperliquid 限价单测试
//!
//! 运行前需要设置环境变量:
//! - HYPERLIQUID_WALLET_ADDRESS (0x...)
//! - HYPERLIQUID_PRIVATE_KEY (不含 0x 前缀)

use fee_arb::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use fee_arb::exchange::hyperliquid::{HyperliquidClient, HyperliquidCredentials};
use fee_arb::exchange::ExchangeClient;
use reqwest::Client;
use serde::Deserialize;

/// 测试交易对
const TEST_BASE: &str = "BTC";
const TEST_QUOTE: &str = "USDC"; // Hyperliquid 使用 USDC

/// 测试数量 - 请根据实际情况填写
const TEST_QUANTITY: f64 = 0.001; // TODO: 填写测试数量

/// 从环境变量获取凭证
fn get_credentials() -> Option<HyperliquidCredentials> {
    let wallet_address = std::env::var("HYPERLIQUID_WALLET_ADDRESS").ok()?;
    let private_key = std::env::var("HYPERLIQUID_PRIVATE_KEY").ok()?;
    Some(HyperliquidCredentials {
        wallet_address,
        private_key,
        quote: TEST_QUOTE.to_string(),
    })
}

/// L2 订单簿响应
#[derive(Debug, Deserialize)]
struct L2Book {
    levels: Vec<Vec<Level>>,
}

#[derive(Debug, Deserialize)]
struct Level {
    px: String,
    sz: String,
    n: u32,
}

/// 获取 BBO (REST API)
async fn fetch_bbo(symbol: &Symbol) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let client = Client::new();
    let coin = &symbol.base; // Hyperliquid 使用 base 作为 coin 名

    let resp: L2Book = client
        .post("https://api.hyperliquid.xyz/info")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "type": "l2Book",
            "coin": coin
        }))
        .send()
        .await?
        .json()
        .await?;

    // levels[0] = bids, levels[1] = asks
    let bids = resp.levels.get(0).ok_or("No bids")?;
    let asks = resp.levels.get(1).ok_or("No asks")?;

    let best_bid = bids.first().ok_or("No bid level")?;
    let best_ask = asks.first().ok_or("No ask level")?;

    let bid: f64 = best_bid.px.parse()?;
    let ask: f64 = best_ask.px.parse()?;

    Ok((bid, ask))
}

/// 限价买单测试 - 使用 ask 价格
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_hyperliquid_limit_buy() {
    let credentials =
        get_credentials().expect("需要设置 HYPERLIQUID_WALLET_ADDRESS 和 HYPERLIQUID_PRIVATE_KEY");
    let client = HyperliquidClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol = Symbol::new(TEST_BASE);

    // 获取 BBO
    let (bid, ask) = fetch_bbo(&symbol).await.expect("获取 BBO 失败");
    println!("BBO: bid={}, ask={}", bid, ask);

    // 获取 SymbolMeta 用于格式化价格
    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    let meta = metas.first().expect("未找到交易对元数据");

    // 格式化价格
    let formatted_price = meta.format_price(ask);
    let price: f64 = formatted_price.parse().expect("价格解析失败");
    println!("下单价格: {} (格式化后: {})", ask, price);

    let cli_id = format!("0x{}", uuid::Uuid::new_v4().simple().to_string());
    // 构造限价买单
    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::Hyperliquid,
        symbol,
        side: Side::Long,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::GTC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: cli_id,
    };

    println!("提交买单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("买单成功，订单ID: {}", order_id);
}

/// 限价卖单测试 - 使用 bid 价格
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_hyperliquid_limit_sell() {
    let credentials =
        get_credentials().expect("需要设置 HYPERLIQUID_WALLET_ADDRESS 和 HYPERLIQUID_PRIVATE_KEY");
    let client = HyperliquidClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol = Symbol::new(TEST_BASE);

    // 获取 BBO
    let (bid, ask) = fetch_bbo(&symbol).await.expect("获取 BBO 失败");
    println!("BBO: bid={}, ask={}", bid, ask);

    // 获取 SymbolMeta 用于格式化价格
    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    let meta = metas.first().expect("未找到交易对元数据");

    // 格式化价格
    let formatted_price = meta.format_price(bid);
    let price: f64 = formatted_price.parse().expect("价格解析失败");
    println!("下单价格: {} (格式化后: {})", bid, price);

    let cli_id = format!("0x{}", uuid::Uuid::new_v4().simple().to_string());
    // 构造限价卖单
    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::Hyperliquid,
        symbol,
        side: Side::Short,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::GTC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: cli_id,
    };

    println!("提交卖单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("卖单成功，订单ID: {}", order_id);
}

//! Binance 限价单测试
//!
//! 运行前需要设置环境变量:
//! - BINANCE_API_KEY
//! - BINANCE_SECRET

use fee_arb::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use fee_arb::exchange::binance::{BinanceClient, BinanceCredentials};
use fee_arb::exchange::ExchangeClient;
use reqwest::Client;
use serde::Deserialize;

/// 测试交易对
const TEST_SYMBOL: (&str, &str) = ("BTC", "USDT");

/// 测试数量 - 请根据实际情况填写
const TEST_QUANTITY: f64 = 0.002; // TODO: 填写测试数量

/// 从环境变量获取凭证
fn get_credentials() -> Option<BinanceCredentials> {
    let api_key = std::env::var("BINANCE_API_KEY").ok()?;
    let secret = std::env::var("BINANCE_SECRET").ok()?;
    Some(BinanceCredentials { api_key, secret })
}

/// BBO 数据
#[derive(Debug, Deserialize)]
struct BboTicker {
    #[serde(rename = "bidPrice")]
    bid_price: String,
    #[serde(rename = "askPrice")]
    ask_price: String,
}

/// 获取 BBO (REST API)
async fn fetch_bbo(symbol: &Symbol) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let client = Client::new();
    let binance_symbol = format!("{}{}", symbol.base, symbol.quote);

    let resp: BboTicker = client
        .get(format!(
            "https://fapi.binance.com/fapi/v1/ticker/bookTicker?symbol={}",
            binance_symbol
        ))
        .send()
        .await?
        .json()
        .await?;

    let bid: f64 = resp.bid_price.parse()?;
    let ask: f64 = resp.ask_price.parse()?;

    Ok((bid, ask))
}

/// 限价买单测试 - 使用 ask 价格
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_binance_limit_buy() {
    let credentials = get_credentials().expect("需要设置 BINANCE_API_KEY 和 BINANCE_SECRET");
    let client = BinanceClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol = Symbol::new(TEST_SYMBOL.0, TEST_SYMBOL.1);

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

    // 构造限价买单
    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::Binance,
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

    // let order_id = client.place_order(order).await.expect("下单失败");
    // println!("买单成功，订单ID: {}", order_id);
}

/// 限价卖单测试 - 使用 bid 价格
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_binance_limit_sell() {
    let credentials = get_credentials().expect("需要设置 BINANCE_API_KEY 和 BINANCE_SECRET");
    let client = BinanceClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol = Symbol::new(TEST_SYMBOL.0, TEST_SYMBOL.1);

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

    // 构造限价卖单
    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::Binance,
        symbol,
        side: Side::Short,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::GTC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: uuid::Uuid::new_v4().to_string(),
    };

    println!("提交卖单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("卖单成功，订单ID: {}", order_id);
}

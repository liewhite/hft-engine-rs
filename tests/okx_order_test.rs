//! OKX 限价单测试
//!
//! 运行前需要设置环境变量:
//! - OKX_API_KEY
//! - OKX_SECRET
//! - OKX_PASSPHRASE

use hft_engine_rs::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use hft_engine_rs::exchange::okx::{OkxClient, OkxCredentials};
use hft_engine_rs::exchange::ExchangeClient;
use reqwest::Client;
use serde::Deserialize;

/// 测试交易对
const TEST_BASE: &str = "BTC";
const TEST_QUOTE: &str = "USDT";

/// 测试数量 - 请根据实际情况填写 (OKX 是张数，1张 = ctVal BTC)
const TEST_QUANTITY: f64 = 0.01; // TODO: 填写测试数量（张数）

/// 从环境变量获取凭证
fn get_credentials() -> Option<OkxCredentials> {
    let api_key = std::env::var("OKX_API_KEY").ok()?;
    let secret = std::env::var("OKX_SECRET").ok()?;
    let passphrase = std::env::var("OKX_PASSPHRASE").ok()?;
    Some(OkxCredentials {
        api_key,
        secret,
        passphrase,
        quote: TEST_QUOTE.to_string(),
    })
}

/// OKX Ticker 响应
#[derive(Debug, Deserialize)]
struct OkxResponse<T> {
    code: String,
    data: Vec<T>,
}

/// OKX Ticker 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TickerData {
    bid_px: String,
    ask_px: String,
}

/// 获取 BBO (REST API)
async fn fetch_bbo(symbol: &Symbol) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let client = Client::new();
    let inst_id = format!("{}-{}-SWAP", symbol, TEST_QUOTE);

    let resp: OkxResponse<TickerData> = client
        .get(format!(
            "https://www.okx.com/api/v5/market/ticker?instId={}",
            inst_id
        ))
        .send()
        .await?
        .json()
        .await?;

    if resp.code != "0" {
        return Err(format!("OKX API error: code={}", resp.code).into());
    }

    let ticker = resp.data.first().ok_or("No ticker data")?;
    let bid: f64 = ticker.bid_px.parse()?;
    let ask: f64 = ticker.ask_px.parse()?;

    Ok((bid, ask))
}

/// 限价买单测试 - 使用 ask 价格
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_okx_limit_buy() {
    let credentials = get_credentials().expect("需要设置 OKX_API_KEY, OKX_SECRET, OKX_PASSPHRASE");
    let client = OkxClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol: Symbol = TEST_BASE.to_string();

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
        id: uuid::Uuid::new_v4().simple().to_string(),
        exchange: Exchange::OKX,
        symbol,
        side: Side::Long,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::IOC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: uuid::Uuid::new_v4().simple().to_string(),
    };

    println!("提交买单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("买单成功，订单ID: {}", order_id);
}

/// 限价卖单测试 - 使用 bid 价格
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_okx_limit_sell() {
    let credentials = get_credentials().expect("需要设置 OKX_API_KEY, OKX_SECRET, OKX_PASSPHRASE");
    let client = OkxClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol: Symbol = TEST_BASE.to_string();

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
        id: uuid::Uuid::new_v4().simple().to_string(),
        exchange: Exchange::OKX,
        symbol,
        side: Side::Short,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::IOC,
        },
        quantity: TEST_QUANTITY,
        reduce_only: false,
        client_order_id: uuid::Uuid::new_v4().simple().to_string(),
    };

    println!("提交卖单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!("卖单成功，订单ID: {}", order_id);
}

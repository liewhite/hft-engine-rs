//! Hyperliquid XYZ DEX 接口测试
//!
//! 测试 XYZ DEX (股票永续合约) 的全部接口。
//!
//! 凭证从 hype_config.json 读取:
//! ```json
//! { "wallet": "0x...", "agent_key": "0x..." }
//! ```
//!
//! 运行:
//! ```sh
//! cargo test --test hyperliquid_xyz_test -- --ignored --nocapture
//! ```

use hft_engine_rs::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use hft_engine_rs::exchange::hyperliquid::{HyperliquidClient, HyperliquidCredentials};
use hft_engine_rs::exchange::ExchangeClient;
use reqwest::Client;
use serde::Deserialize;

/// 测试交易对 (XYZ DEX 上的 NVDA 股票永续)
const TEST_COIN: &str = "xyz:NVDA";
const TEST_QUOTE: &str = "USDC";
const TEST_DEX: &str = "xyz";

/// hype_config.json 文件格式
#[derive(Debug, Deserialize)]
struct HypeConfig {
    wallet: String,
    agent_key: String,
}

/// 从 hype_config.json 加载凭证
fn load_credentials() -> HyperliquidCredentials {
    let content = std::fs::read_to_string("hype_config.json")
        .expect("读取 hype_config.json 失败，请确认文件存在");
    let config: HypeConfig =
        serde_json::from_str(&content).expect("解析 hype_config.json 失败");
    HyperliquidCredentials {
        wallet_address: config.wallet,
        private_key: config.agent_key,
        quote: TEST_QUOTE.to_string(),
        dex: TEST_DEX.to_string(),
    }
}

/// 创建 XYZ DEX 客户端（无凭证，仅用于公开接口）
fn create_public_client() -> HyperliquidClient {
    let creds = HyperliquidCredentials {
        wallet_address: "0x0000000000000000000000000000000000000000".to_string(),
        private_key: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        quote: TEST_QUOTE.to_string(),
        dex: TEST_DEX.to_string(),
    };
    HyperliquidClient::new(Some(creds)).expect("创建公开客户端失败")
}

/// L2 订单簿响应
#[derive(Debug, Deserialize)]
struct L2Book {
    levels: Vec<Vec<Level>>,
}

#[derive(Debug, Deserialize)]
struct Level {
    px: String,
    #[allow(dead_code)]
    sz: String,
    #[allow(dead_code)]
    n: u32,
}

/// 获取 BBO (REST API)
async fn fetch_bbo(coin: &str) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let client = Client::new();
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

    let bids = resp.levels.get(0).ok_or("No bids")?;
    let asks = resp.levels.get(1).ok_or("No asks")?;

    let best_bid = bids.first().ok_or("No bid level")?;
    let best_ask = asks.first().ok_or("No ask level")?;

    let bid: f64 = best_bid.px.parse()?;
    let ask: f64 = best_ask.px.parse()?;

    Ok((bid, ask))
}

// ============================================================================
// 公开接口测试
// ============================================================================

/// 测试获取 XYZ DEX 所有交易对元数据
#[tokio::test]
#[ignore = "需要网络"]
async fn test_xyz_fetch_all_symbol_metas() {
    let client = create_public_client();
    let metas = client
        .fetch_all_symbol_metas()
        .await
        .expect("获取所有 SymbolMeta 失败");

    println!("XYZ DEX 交易对数量: {}", metas.len());
    assert!(!metas.is_empty(), "XYZ DEX 应该有交易对");

    // 检查是否包含 NVDA
    let nvda = metas.iter().find(|m| m.symbol.contains("NVDA"));
    assert!(nvda.is_some(), "XYZ DEX 应该包含 NVDA");

    // 打印前 10 个
    for meta in metas.iter().take(10) {
        println!(
            "  {} | size_step={} | min_order_size={} | contract_size={}",
            meta.symbol, meta.size_step, meta.min_order_size, meta.contract_size
        );
    }
}

/// 测试获取指定 XYZ DEX symbol 的元数据
#[tokio::test]
#[ignore = "需要网络"]
async fn test_xyz_fetch_symbol_meta() {
    let client = create_public_client();
    let symbol: Symbol = TEST_COIN.to_string();

    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");

    assert_eq!(metas.len(), 1, "应该返回 1 个 SymbolMeta");
    let meta = &metas[0];
    println!(
        "{}: size_step={}, min_order_size={}, contract_size={}",
        meta.symbol, meta.size_step, meta.min_order_size, meta.contract_size
    );
    assert_eq!(meta.exchange, Exchange::Hyperliquid);
    assert!(meta.symbol.contains("NVDA"));

    // 测试价格格式化
    let test_price = 120.567;
    let formatted = meta.format_price(test_price);
    println!("价格格式化: {} -> {}", test_price, formatted);
}

/// 测试获取 XYZ DEX BBO
#[tokio::test]
#[ignore = "需要网络"]
async fn test_xyz_fetch_bbo() {
    let (bid, ask) = fetch_bbo(TEST_COIN).await.expect("获取 BBO 失败");

    println!("{} BBO: bid={}, ask={}, spread={:.4}", TEST_COIN, bid, ask, ask - bid);
    assert!(bid > 0.0, "bid 应该大于 0");
    assert!(ask > 0.0, "ask 应该大于 0");
    assert!(ask >= bid, "ask 应该 >= bid");
}

/// 测试获取 XYZ DEX meta 和 asset contexts
#[tokio::test]
#[ignore = "需要网络"]
async fn test_xyz_fetch_meta_and_asset_ctxs() {
    let http = Client::new();
    let resp: serde_json::Value = http
        .post("https://api.hyperliquid.xyz/info")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({"type": "metaAndAssetCtxs", "dex": TEST_DEX}))
        .send()
        .await
        .expect("请求失败")
        .json()
        .await
        .expect("JSON 解析失败");

    // resp[0] = meta, resp[1] = assetCtxs
    let meta = &resp[0];
    let ctxs = &resp[1];

    let universe = meta["universe"].as_array().expect("universe 应该是数组");
    let ctxs_arr = ctxs.as_array().expect("assetCtxs 应该是数组");

    println!("XYZ DEX assets: {}, contexts: {}", universe.len(), ctxs_arr.len());
    assert_eq!(
        universe.len(),
        ctxs_arr.len(),
        "universe 和 assetCtxs 数量应一致"
    );

    // 找到 NVDA
    for (i, asset) in universe.iter().enumerate() {
        let name = asset["name"].as_str().unwrap_or("");
        if name.contains("NVDA") {
            let ctx = &ctxs_arr[i];
            println!("NVDA (index {}): {:?}", i, asset);
            println!("NVDA ctx: {:?}", ctx);
            println!(
                "  mark_px={}, oracle_px={}, funding={}, mid_px={}",
                ctx["markPx"], ctx["oraclePx"], ctx["funding"], ctx["midPx"]
            );
            break;
        }
    }
}

// ============================================================================
// 交易接口测试
// ============================================================================

/// 下单测试辅助函数
async fn place_test_order(side: Side) {
    let credentials = load_credentials();
    let client = HyperliquidClient::new(Some(credentials)).expect("创建客户端失败");

    let symbol: Symbol = TEST_COIN.to_string();

    // 获取 BBO
    let (bid, ask) = fetch_bbo(TEST_COIN).await.expect("获取 BBO 失败");
    println!("{} BBO: bid={}, ask={}", TEST_COIN, bid, ask);

    // 获取 SymbolMeta 用于格式化价格
    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    let meta = metas.first().expect("未找到交易对元数据");
    println!(
        "SymbolMeta: size_step={}, min_order_size={}",
        meta.size_step, meta.min_order_size
    );

    // 做多用 ask 价格，做空用 bid 价格（立即成交）
    let raw_price = match side {
        Side::Long => ask,
        Side::Short => bid,
    };
    let formatted_price = meta.format_price(raw_price);
    let price: f64 = formatted_price.parse().expect("价格解析失败");
    println!("下单价格: {} (格式化后: {})", raw_price, price);

    // XYZ DEX 股票永续有最小 notional 要求，使用 1 股
    let quantity = 1.0_f64.max(meta.min_order_size);
    println!("下单数量: {}", quantity);

    let cli_id = Exchange::Hyperliquid.new_cli_order_id();
    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::Hyperliquid,
        symbol,
        side,
        order_type: OrderType::Limit {
            price,
            tif: TimeInForce::GTC,
        },
        quantity,
        reduce_only: false,
        client_order_id: cli_id.clone(),
    };

    println!("提交订单: {:?}", order);

    let order_id = client.place_order(order).await.expect("下单失败");
    println!(
        "下单成功，方向={:?}，订单ID: {}, 客户端ID: {}",
        side, order_id, cli_id
    );
}

/// 限价买单 (做多) — 使用 ask 价格，立即成交
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_xyz_limit_buy() {
    place_test_order(Side::Long).await;
}

/// 限价卖单 (做空) — 使用 bid 价格，立即成交
#[tokio::test]
#[ignore = "需要真实凭证和网络"]
async fn test_xyz_limit_sell() {
    place_test_order(Side::Short).await;
}

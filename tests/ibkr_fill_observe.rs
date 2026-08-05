//! IBKR Fill 推送观测测试
//!
//! 直接连接 WebSocket，订阅 sor + str topic，下单后记录所有原始消息，
//! 用于观察：
//! - 每笔 fill 推送几条消息
//! - 哪条带 commission
//! - sor 推送是否带 fee
//!
//! 运行: cargo test ibkr_fill_observe -- --ignored --nocapture

use hft_engine_rs::domain::{Exchange, Order, OrderType, Side, TimeInForce};
use hft_engine_rs::exchange::ibkr::{IbkrClient, IbkrCredentials};
use hft_engine_rs::exchange::ibkr::auth::{tickle, IbkrAuth};
use hft_engine_rs::exchange::{ExchangeClient, ExchangeOrder};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::{handshake::client::generate_key, http, Message as WsMessage};

/// 从项目根目录 config.json 的 ibkr 字段读取凭证
fn load_credentials() -> IbkrCredentials {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config.json");
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path, e));
    let config: serde_json::Value = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("解析 config.json 失败: {}", e));
    let ibkr = config.get("ibkr").expect("config.json 中缺少 ibkr 字段");
    serde_json::from_value(ibkr.clone())
        .unwrap_or_else(|e| panic!("解析 ibkr 凭证失败: {}", e))
}

/// 连接 IBKR WebSocket，返回 (write, read)
async fn connect_ws(
    auth: &dyn IbkrAuth,
    session_id: &str,
) -> (
    impl SinkExt<WsMessage> + Unpin,
    impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
) {
    let ws_url = auth.ws_url();
    let connector = auth.ws_connector().expect("ws_connector build failed");
    let cookie = auth.format_ws_cookie(session_id);
    let uri: http::Uri = ws_url.parse().expect("Invalid WS URL");
    let host = uri.host().expect("WS URL missing host").to_string();
    let ws_key = generate_key();
    let ws_request = http::Request::builder()
        .uri(&ws_url)
        .header("Host", &host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", &ws_key)
        .header("Cookie", &cookie)
        .header("User-Agent", "ClientPortalGW/1")
        .body(())
        .expect("Failed to build WS request");

    let (ws_stream, _) = match connector {
        Some(conn) => {
            tokio_tungstenite::connect_async_tls_with_config(ws_request, None, false, Some(conn))
                .await
                .expect("WS connect failed")
        }
        None => tokio_tungstenite::connect_async(ws_request)
            .await
            .expect("WS connect failed"),
    };

    ws_stream.split()
}

#[tokio::test]
#[ignore = "需要真实凭证和网络，会产生真实交易"]
async fn ibkr_fill_observe() {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init()
        .ok();

    let credentials = load_credentials();
    let symbol = credentials.symbols().first().expect("至少配置一个 symbol").clone();

    // 1. 创建 client（获取 auth + conids）
    let client = IbkrClient::new(&credentials).await.expect("IBKR 连接失败");
    let auth = client.auth();
    let http_client = auth.build_http_client().expect("创建 HTTP 客户端失败");

    // 2. tickle 获取 session_id
    let session_id = tickle(&*auth, &http_client).await.expect("tickle 失败");
    println!("session_id: {}", session_id);

    // 3. 直接连接 WebSocket
    let (mut write, mut read) = connect_ws(&*auth, &session_id).await;
    println!("WebSocket 已连接");

    // 4. 订阅 sor + str
    write
        .send(WsMessage::Text("sor+{}".to_string()))
        .await
        .ok()
        .expect("发送 sor 订阅失败");
    write
        .send(WsMessage::Text("str+{}".to_string()))
        .await
        .ok()
        .expect("发送 str 订阅失败");
    println!("已订阅 sor + str");

    // 5. 等 5 秒让订阅生效，消费掉初始消息
    let drain_until = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut init_count = 0u32;
    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        init_count += 1;
                        println!("[初始消息 #{}] {}", init_count, text);
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await.ok();
                        println!("[初始] 收到 Ping, 已回 Pong");
                    }
                    Some(Ok(other)) => {
                        println!("[初始] 其他消息类型: {:?}", other);
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(drain_until) => break,
        }
    }
    println!("初始阶段收到 {} 条消息\n", init_count);

    // 6. 获取 BBO 并下单
    let (bid, ask) = client.fetch_snapshot_bbo(&symbol).await.expect("获取 BBO 失败");
    println!("\n{} bid={} ask={}", symbol, bid, ask);

    let metas = client
        .fetch_symbol_meta(&[symbol.clone()])
        .await
        .expect("获取 SymbolMeta 失败");
    let meta = metas.first().expect("未找到 SymbolMeta");

    // IOC 买入 1 股，价格吃 ask + 滑点确保成交
    let buy_price: f64 = meta.format_price(ask + 0.05).parse().unwrap();
    let client_oid = Exchange::IBKR.new_cli_order_id();
    let order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::IBKR,
        symbol: symbol.clone(),
        side: Side::Long,
        order_type: OrderType::Limit {
            price: buy_price,
            tif: TimeInForce::IOC,
        },
        quantity: 1.0,
        reduce_only: false,
        client_order_id: client_oid.clone(),
    };

    println!("\n下单买入: {} @ {} cOID={}", symbol, buy_price, client_oid);
    let order_id = client.place_order(ExchangeOrder::from_domain(order, &meta)).await.expect("下单失败");
    println!("下单成功 order_id={}", order_id);

    // 7. 监听所有 WS 消息 30 秒，打印 sor/str 相关的原始 JSON
    println!("\n========== 开始监听 WS 推送 (30s) ==========\n");

    let mut msg_count = 0u32;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);

    loop {
        tokio::select! {
            msg = read.next() => {
                // 提取文本（Text 或 Binary→UTF-8）
                let text = match &msg {
                    Some(Ok(WsMessage::Text(t))) => Some(t.clone()),
                    Some(Ok(WsMessage::Binary(b))) => String::from_utf8(b.clone().into()).ok(),
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data.clone())).await.ok();
                        None
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        println!("WS 关闭: {:?}", frame);
                        break;
                    }
                    None => {
                        println!("WS 连接断开");
                        break;
                    }
                    _ => None,
                };

                if let Some(text) = text {
                    msg_count += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S%.3f");

                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                        let topic = val.get("topic").and_then(|v| v.as_str()).unwrap_or("?");
                        let pretty = serde_json::to_string_pretty(&val).unwrap();
                        println!("[{}] #{} topic={}\n{}\n", ts, msg_count, topic, pretty);
                    } else {
                        println!("[{}] #{} (非JSON) {}", ts, msg_count, text);
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                println!("\n========== 30s 超时，停止监听 ==========");
                break;
            }
        }
    }

    println!("\n共收到 {} 条消息", msg_count);

    // 8. 平仓卖出
    let (bid2, _) = client.fetch_snapshot_bbo(&symbol).await.expect("获取 BBO 失败");
    let sell_price: f64 = meta.format_price(bid2 - 0.05).parse().unwrap();
    let sell_order = Order {
        id: uuid::Uuid::new_v4().to_string(),
        exchange: Exchange::IBKR,
        symbol: symbol.clone(),
        side: Side::Short,
        order_type: OrderType::Limit {
            price: sell_price,
            tif: TimeInForce::IOC,
        },
        quantity: 1.0,
        reduce_only: false,
        client_order_id: Exchange::IBKR.new_cli_order_id(),
    };
    println!("\n平仓卖出 {} @ {}", symbol, sell_price);
    let _ = client.place_order(ExchangeOrder::from_domain(sell_order, &meta)).await.expect("卖单失败");
    println!("平仓完成");
}

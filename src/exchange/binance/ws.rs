use crate::domain::{Exchange, ExchangeError, Symbol};
use crate::exchange::api::{ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::binance::rest::BinanceRestClient;
use crate::exchange::binance::WS_PUBLIC_URL;
use crate::exchange::ws_util::{ExponentialBackoff, RetryConfig};
use crate::parse_or_panic;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// Binance WebSocket 客户端
pub struct BinanceWebSocket {
    rest_client: Arc<BinanceRestClient>,
    listen_key: Arc<Mutex<Option<String>>>,
}

impl BinanceWebSocket {
    pub fn new(api_key: String, secret: String) -> Result<Self, ExchangeError> {
        Ok(Self {
            rest_client: Arc::new(BinanceRestClient::new(api_key, secret)?),
            listen_key: Arc::new(Mutex::new(None)),
        })
    }

    /// 启动 ListenKey 保活任务
    fn start_keepalive(&self, cancel_token: CancellationToken) -> JoinHandle<()> {
        let client = self.rest_client.clone();

        tokio::spawn(async move {
            // 首次延迟 30 分钟后再开始保活，避免在 listen key 创建前就调用
            let start = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
            let mut interval = tokio::time::interval_at(start, Duration::from_secs(30 * 60));

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(e) = client.keep_alive_listen_key().await {
                            tracing::error!(error = %e, "Failed to keep alive listen key");
                        } else {
                            tracing::debug!("Binance listen key kept alive");
                        }
                    }
                }
            }
        })
    }
}

#[async_trait]
impl ExchangeWebSocket for BinanceWebSocket {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn connect_public(
        &self,
        sinks: PublicSinks,
        cancel_token: CancellationToken,
    ) -> Result<(), ExchangeError> {
        let symbols = sinks.symbols();
        if symbols.is_empty() {
            return Ok(());
        }

        // 构建订阅参数
        let mut streams: Vec<String> = Vec::new();
        for symbol in &symbols {
            let s = symbol.to_binance().to_lowercase();
            streams.push(format!("{}@markPrice@1s", s));
            streams.push(format!("{}@bookTicker", s));
        }

        let subscribe_msg = json!({
            "method": "SUBSCRIBE",
            "params": streams,
            "id": 1
        });

        // 启动带重连的消息处理任务
        tokio::spawn(async move {
            let mut backoff = ExponentialBackoff::new(RetryConfig::default());

            loop {
                if cancel_token.is_cancelled() {
                    break;
                }

                tracing::info!(url = %WS_PUBLIC_URL, "Connecting to Binance public WebSocket");

                match connect_async(WS_PUBLIC_URL).await {
                    Ok((ws_stream, _)) => {
                        let (mut write, mut read) = ws_stream.split();

                        // 发送订阅消息
                        if let Err(e) = write.send(Message::Text(subscribe_msg.to_string())).await {
                            tracing::error!(error = %e, "Failed to send subscribe message");
                            backoff.wait().await;
                            continue;
                        }

                        // 等待订阅响应
                        let subscribe_ok = match read.next().await {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<WsResponse>(&text) {
                                    Ok(resp) if resp.id == 1 => {
                                        if let Some(err) = resp.error {
                                            tracing::error!(
                                                code = err.code,
                                                msg = %err.msg,
                                                "Binance subscribe failed"
                                            );
                                            false
                                        } else {
                                            tracing::info!("Binance public subscribe successful");
                                            true
                                        }
                                    }
                                    Ok(resp) => {
                                        tracing::error!(
                                            id = resp.id,
                                            "Unexpected response id while waiting for subscribe"
                                        );
                                        false
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = %e,
                                            raw = %text,
                                            "Failed to parse subscribe response"
                                        );
                                        false
                                    }
                                }
                            }
                            Some(Ok(other)) => {
                                tracing::error!(msg = ?other, "Unexpected message type while waiting for subscribe");
                                false
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "WebSocket error while waiting for subscribe");
                                false
                            }
                            None => {
                                tracing::error!("WebSocket closed while waiting for subscribe");
                                false
                            }
                        };

                        if !subscribe_ok {
                            backoff.wait().await;
                            continue;
                        }

                        backoff.reset();

                        // 消息处理循环
                        loop {
                            tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    let _ = write.close().await;
                                    return;
                                }
                                msg = read.next() => {
                                    match msg {
                                        Some(Ok(Message::Text(text))) => {
                                            handle_binance_public_message(
                                                &text,
                                                &sinks.funding_rates,
                                                &sinks.bbos,
                                            );
                                        }
                                        Some(Ok(Message::Ping(data))) => {
                                            if let Err(e) = write.send(Message::Pong(data)).await {
                                                tracing::error!(error = %e, "Failed to send pong");
                                                break;
                                            }
                                        }
                                        Some(Ok(Message::Close(_))) | None => {
                                            tracing::warn!("Binance public WebSocket closed, will reconnect");
                                            break;
                                        }
                                        Some(Err(e)) => {
                                            tracing::error!(error = %e, "Binance WebSocket error, will reconnect");
                                            break;
                                        }
                                        Some(Ok(Message::Pong(_))) => {
                                            // Pong 响应，正常忽略
                                        }
                                        Some(Ok(Message::Binary(data))) => {
                                            panic!("Unexpected binary message from Binance public WebSocket: {} bytes", data.len());
                                        }
                                        Some(Ok(Message::Frame(_))) => {
                                            panic!("Unexpected raw frame from Binance public WebSocket");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to connect to Binance public WebSocket");
                    }
                }

                // 重试前等待
                if cancel_token.is_cancelled() {
                    break;
                }
                backoff.wait().await;
            }
        });

        Ok(())
    }

    async fn connect_private(
        &self,
        sinks: PrivateSinks,
        cancel_token: CancellationToken,
    ) -> Result<(), ExchangeError> {
        // 启动保活
        self.start_keepalive(cancel_token.clone());

        let rest_client = self.rest_client.clone();
        let listen_key_holder = self.listen_key.clone();

        // 启动带重连的消息处理任务
        tokio::spawn(async move {
            let mut backoff = ExponentialBackoff::new(RetryConfig::default());

            loop {
                if cancel_token.is_cancelled() {
                    break;
                }

                // 每次重连都需要获取新的 ListenKey
                let listen_key = match rest_client.create_listen_key().await {
                    Ok(key) => {
                        *listen_key_holder.lock().await = Some(key.clone());
                        key
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to create listen key");
                        backoff.wait().await;
                        continue;
                    }
                };

                let url = format!("wss://fstream.binance.com/ws/{}", listen_key);
                tracing::info!(url = %url, "Connecting to Binance private WebSocket");

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        backoff.reset();
                        let (mut write, mut read) = ws_stream.split();

                        // 消息处理循环
                        loop {
                            tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    let _ = write.close().await;
                                    return;
                                }
                                msg = read.next() => {
                                    match msg {
                                        Some(Ok(Message::Text(text))) => {
                                            if !handle_binance_private_message(
                                                &text,
                                                &sinks.positions,
                                                &sinks.balances,
                                                &sinks.order_updates,
                                            ) {
                                                // 需要重连
                                                break;
                                            }
                                        }
                                        Some(Ok(Message::Ping(data))) => {
                                            if let Err(e) = write.send(Message::Pong(data)).await {
                                                tracing::error!(error = %e, "Failed to send pong");
                                                break;
                                            }
                                        }
                                        Some(Ok(Message::Close(_))) | None => {
                                            tracing::warn!("Binance private WebSocket closed, will reconnect");
                                            break;
                                        }
                                        Some(Err(e)) => {
                                            tracing::error!(error = %e, "Binance private WebSocket error, will reconnect");
                                            break;
                                        }
                                        Some(Ok(Message::Pong(_))) => {
                                            // Pong 响应，正常忽略
                                        }
                                        Some(Ok(Message::Binary(data))) => {
                                            panic!("Unexpected binary message from Binance private WebSocket: {} bytes", data.len());
                                        }
                                        Some(Ok(Message::Frame(_))) => {
                                            panic!("Unexpected raw frame from Binance private WebSocket");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to connect to Binance private WebSocket");
                    }
                }

                // 重试前等待
                if cancel_token.is_cancelled() {
                    break;
                }
                backoff.wait().await;
            }
        });

        Ok(())
    }
}

/// 处理 Binance 公共消息
///
/// 解析失败时 panic，因为这表示代码逻辑漏洞
fn handle_binance_public_message(
    text: &str,
    funding_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::FundingRate>>,
    bbo_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::BBO>>,
) {
    // 先解析为 Value 获取事件类型
    let value: serde_json::Value = parse_or_panic!(text, serde_json::Value, "Binance public base");
    let event_type = value.get("e").and_then(|e| e.as_str());

    match event_type {
        Some("markPriceUpdate") => {
            let update: MarkPriceUpdate = parse_or_panic!(text, MarkPriceUpdate, "markPriceUpdate");
            let rate = update.to_funding_rate();
            // 按 symbol 路由到对应的 sink (未订阅的 symbol 忽略)
            if let Some(tx) = funding_sinks.get(&rate.symbol) {
                let _ = tx.send(rate);
            }
        }
        Some("bookTicker") => {
            let ticker: BookTicker = parse_or_panic!(text, BookTicker, "bookTicker");
            let bbo = ticker.to_bbo();
            // 按 symbol 路由到对应的 sink (未订阅的 symbol 忽略)
            if let Some(tx) = bbo_sinks.get(&bbo.symbol) {
                let _ = tx.send(bbo);
            }
        }
        Some(unknown) => {
            panic!("Received unknown Binance public event type: {}\nRaw: {}", unknown, text);
        }
        None => {
            panic!("Binance public message missing event type 'e'\nRaw: {}", text);
        }
    }
}

/// 处理 Binance 私有消息
///
/// 解析失败时 panic，因为这表示代码逻辑漏洞
/// 返回 false 表示需要重连
fn handle_binance_private_message(
    text: &str,
    position_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::Position>>,
    balance_sink: &broadcast::Sender<crate::domain::Balance>,
    order_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::OrderUpdate>>,
) -> bool {
    // 先解析为 Value 获取事件类型
    let value: serde_json::Value = parse_or_panic!(text, serde_json::Value, "Binance private base");
    let event_type = value.get("e").and_then(|e| e.as_str());

    match event_type {
        Some("ACCOUNT_UPDATE") => {
            let update: AccountUpdate = parse_or_panic!(text, AccountUpdate, "ACCOUNT_UPDATE");
            // 发送仓位更新 - 按 symbol 路由 (未订阅的 symbol 忽略)
            for pos in &update.a.positions {
                let position = pos.to_position();
                if let Some(tx) = position_sinks.get(&position.symbol) {
                    let _ = tx.send(position);
                }
            }
            // 发送余额更新 - 不按 symbol 分
            for bal in &update.a.balances {
                let balance = bal.to_balance();
                let _ = balance_sink.send(balance);
            }
        }
        Some("ORDER_TRADE_UPDATE") => {
            let update: OrderTradeUpdate = parse_or_panic!(text, OrderTradeUpdate, "ORDER_TRADE_UPDATE");
            let order_update = update.to_order_update();
            // 按 symbol 路由 (未订阅的 symbol 忽略)
            if let Some(tx) = order_sinks.get(&order_update.symbol) {
                let _ = tx.send(order_update);
            }
        }
        Some("listenKeyExpired") => {
            // ListenKey 过期，返回 false 触发重连
            tracing::warn!("Binance ListenKey expired, will reconnect");
            return false;
        }
        Some(unknown) => {
            panic!("Received unknown Binance private event type: {}\nRaw: {}", unknown, text);
        }
        None => {
            panic!("Binance private message missing event type 'e'\nRaw: {}", text);
        }
    }

    true
}

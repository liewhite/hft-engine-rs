use crate::domain::{Exchange, ExchangeError, Symbol};
use crate::exchange::api::{ExchangeWebSocket, PrivateHubs, PublicHubs};
use crate::exchange::binance::codec::{AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate};
use crate::exchange::binance::rest::BinanceRestClient;
use crate::exchange::binance::WS_PUBLIC_URL;
use crate::exchange::ws_util::{ExponentialBackoff, RetryConfig};
use crate::parse_or_panic;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
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
            let mut interval = tokio::time::interval(Duration::from_secs(30 * 60)); // 30分钟

            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(e) = client.keep_alive_listen_key().await {
                            tracing::error!(error = %e, "Failed to keep alive listen key");
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
        symbols: &[Symbol],
        cancel_token: CancellationToken,
    ) -> Result<PublicHubs, ExchangeError> {
        let hubs = PublicHubs::new(1024);

        // 构建订阅参数
        let mut streams: Vec<String> = Vec::new();
        for symbol in symbols {
            let s = symbol.to_binance().to_lowercase();
            streams.push(format!("{}@markPrice@1s", s));
            streams.push(format!("{}@bookTicker", s));
        }

        let url = format!("{}/{}", WS_PUBLIC_URL, streams.join("/"));
        let funding_tx = hubs.funding_rates.clone();
        let bbo_tx = hubs.bbos.clone();

        // 启动带重连的消息处理任务
        tokio::spawn(async move {
            let mut backoff = ExponentialBackoff::new(RetryConfig::default());

            loop {
                if cancel_token.is_cancelled() {
                    break;
                }

                tracing::info!(url = %url, "Connecting to Binance public WebSocket");

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
                                            handle_binance_public_message(&text, &funding_tx, &bbo_tx);
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
                                        _ => {}
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

        Ok(hubs)
    }

    async fn connect_private(
        &self,
        cancel_token: CancellationToken,
    ) -> Result<PrivateHubs, ExchangeError> {
        let hubs = PrivateHubs::new(256);

        // 启动保活
        self.start_keepalive(cancel_token.clone());

        let rest_client = self.rest_client.clone();
        let listen_key_holder = self.listen_key.clone();
        let position_tx = hubs.positions.clone();
        let balance_tx = hubs.balances.clone();
        let order_tx = hubs.order_updates.clone();

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
                                            handle_binance_private_message(&text, &position_tx, &balance_tx, &order_tx);
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
                                        _ => {}
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

        Ok(hubs)
    }
}

/// 处理 Binance 公共消息
///
/// 解析失败时 panic，因为这表示代码逻辑漏洞
fn handle_binance_public_message(
    text: &str,
    funding_tx: &tokio::sync::broadcast::Sender<crate::domain::FundingRate>,
    bbo_tx: &tokio::sync::broadcast::Sender<crate::domain::BBO>,
) {
    // 先解析为 Value 获取事件类型
    let value: serde_json::Value = parse_or_panic!(text, serde_json::Value, "Binance public base");
    let event_type = value.get("e").and_then(|e| e.as_str());

    match event_type {
        Some("markPriceUpdate") => {
            let update: MarkPriceUpdate = parse_or_panic!(text, MarkPriceUpdate, "markPriceUpdate");
            if let Some(rate) = update.to_funding_rate() {
                let _ = funding_tx.send(rate);
            }
        }
        Some("bookTicker") => {
            let ticker: BookTicker = parse_or_panic!(text, BookTicker, "bookTicker");
            if let Some(bbo) = ticker.to_bbo() {
                let _ = bbo_tx.send(bbo);
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
fn handle_binance_private_message(
    text: &str,
    position_tx: &tokio::sync::broadcast::Sender<crate::domain::Position>,
    balance_tx: &tokio::sync::broadcast::Sender<crate::domain::Balance>,
    order_tx: &tokio::sync::broadcast::Sender<crate::domain::OrderUpdate>,
) {
    // 先解析为 Value 获取事件类型
    let value: serde_json::Value = parse_or_panic!(text, serde_json::Value, "Binance private base");
    let event_type = value.get("e").and_then(|e| e.as_str());

    match event_type {
        Some("ACCOUNT_UPDATE") => {
            let update: AccountUpdate = parse_or_panic!(text, AccountUpdate, "ACCOUNT_UPDATE");
            // 发送仓位更新
            for pos in &update.a.positions {
                if let Some(position) = pos.to_position() {
                    let _ = position_tx.send(position);
                }
            }
            // 发送余额更新
            for bal in &update.a.balances {
                if let Some(balance) = bal.to_balance() {
                    let _ = balance_tx.send(balance);
                }
            }
        }
        Some("ORDER_TRADE_UPDATE") => {
            let update: OrderTradeUpdate = parse_or_panic!(text, OrderTradeUpdate, "ORDER_TRADE_UPDATE");
            if let Some(order_update) = update.to_order_update() {
                let _ = order_tx.send(order_update);
            }
        }
        Some("listenKeyExpired") => {
            // ListenKey 过期，触发重连
            panic!("Binance ListenKey expired, should reconnect\nRaw: {}", text);
        }
        Some(unknown) => {
            panic!("Received unknown Binance private event type: {}\nRaw: {}", unknown, text);
        }
        None => {
            panic!("Binance private message missing event type 'e'\nRaw: {}", text);
        }
    }
}

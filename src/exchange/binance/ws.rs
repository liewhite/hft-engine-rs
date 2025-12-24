use crate::domain::{Exchange, ExchangeError, Order, OrderId, Symbol};
use crate::exchange::api::{ExchangeExecutor, ExchangeWebSocket, PrivateHubs, PublicHubs};
use crate::exchange::binance::codec::{AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate};
use crate::exchange::binance::rest::BinanceRestClient;
use crate::exchange::binance::WS_PUBLIC_URL;
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
    pub fn new(api_key: String, secret: String) -> Self {
        Self {
            rest_client: Arc::new(BinanceRestClient::new(api_key, secret)),
            listen_key: Arc::new(Mutex::new(None)),
        }
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
        tracing::info!(url = %url, "Connecting to Binance public WebSocket");

        let (ws_stream, _) = connect_async(&url)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        let funding_tx = hubs.funding_rates.clone();
        let bbo_tx = hubs.bbos.clone();

        // 处理消息
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        let _ = write.close().await;
                        break;
                    }
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                // 尝试解析为 MarkPrice
                                if let Ok(update) = serde_json::from_str::<MarkPriceUpdate>(&text) {
                                    if update.e == "markPriceUpdate" {
                                        if let Some(rate) = update.to_funding_rate() {
                                            let _ = funding_tx.send(rate);
                                        }
                                    }
                                }
                                // 尝试解析为 BookTicker
                                else if let Ok(ticker) = serde_json::from_str::<BookTicker>(&text) {
                                    if ticker.e == "bookTicker" {
                                        if let Some(bbo) = ticker.to_bbo() {
                                            let _ = bbo_tx.send(bbo);
                                        }
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::warn!("Binance public WebSocket closed");
                                break;
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "Binance WebSocket error");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(hubs)
    }

    async fn connect_private(
        &self,
        cancel_token: CancellationToken,
    ) -> Result<PrivateHubs, ExchangeError> {
        let hubs = PrivateHubs::new(256);

        // 创建 ListenKey
        let listen_key = self.rest_client.create_listen_key().await?;
        *self.listen_key.lock().await = Some(listen_key.clone());

        // 启动保活
        self.start_keepalive(cancel_token.clone());

        let url = format!("wss://fstream.binance.com/ws/{}", listen_key);
        tracing::info!(url = %url, "Connecting to Binance private WebSocket");

        let (ws_stream, _) = connect_async(&url)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        let position_tx = hubs.positions.clone();
        let balance_tx = hubs.balances.clone();
        let order_tx = hubs.order_updates.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        let _ = write.close().await;
                        break;
                    }
                    msg = read.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                // 解析事件类型
                                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                                    let event_type = value.get("e").and_then(|e| e.as_str()).unwrap_or("");

                                    match event_type {
                                        "ACCOUNT_UPDATE" => {
                                            if let Ok(update) = serde_json::from_str::<AccountUpdate>(&text) {
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
                                        }
                                        "ORDER_TRADE_UPDATE" => {
                                            if let Ok(update) = serde_json::from_str::<OrderTradeUpdate>(&text) {
                                                if let Some(order_update) = update.to_order_update() {
                                                    let _ = order_tx.send(order_update);
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::warn!("Binance private WebSocket closed");
                                break;
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "Binance private WebSocket error");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(hubs)
    }
}

#[async_trait]
impl ExchangeExecutor for BinanceWebSocket {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
        self.rest_client.place_order(order).await
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        self.rest_client.set_leverage(symbol, leverage).await
    }
}

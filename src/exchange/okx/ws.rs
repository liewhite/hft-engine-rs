use crate::domain::{Exchange, ExchangeError, Order, OrderId, Symbol};
use crate::exchange::api::{ExchangeExecutor, ExchangeWebSocket, PrivateHubs, PublicHubs};
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsEvent, WsPush,
};
use crate::exchange::okx::rest::OkxRestClient;
use crate::exchange::okx::{WS_PRIVATE_URL, WS_PUBLIC_URL};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// OKX WebSocket 客户端
pub struct OkxWebSocket {
    rest_client: Arc<OkxRestClient>,
}

impl OkxWebSocket {
    pub fn new(api_key: String, secret: String, passphrase: String) -> Result<Self, ExchangeError> {
        Ok(Self {
            rest_client: Arc::new(OkxRestClient::new(api_key, secret, passphrase)?),
        })
    }
}

#[async_trait]
impl ExchangeWebSocket for OkxWebSocket {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn connect_public(
        &self,
        symbols: &[Symbol],
        cancel_token: CancellationToken,
    ) -> Result<PublicHubs, ExchangeError> {
        let hubs = PublicHubs::new(1024);

        tracing::info!(url = %WS_PUBLIC_URL, "Connecting to OKX public WebSocket");

        let (ws_stream, _) = connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 构建订阅消息
        let mut args = Vec::new();
        for symbol in symbols {
            let inst_id = symbol.to_okx();
            args.push(json!({
                "channel": "funding-rate",
                "instId": inst_id
            }));
            args.push(json!({
                "channel": "bbo-tbt",
                "instId": inst_id
            }));
        }

        let subscribe_msg = json!({
            "op": "subscribe",
            "args": args
        });

        write
            .send(Message::Text(subscribe_msg.to_string()))
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        let funding_tx = hubs.funding_rates.clone();
        let bbo_tx = hubs.bbos.clone();

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
                                // 检查是否是事件响应
                                if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                                    if event.event == "subscribe" {
                                        tracing::debug!(channel = %text, "OKX subscription confirmed");
                                        continue;
                                    }
                                }

                                // 尝试解析 funding-rate
                                if let Ok(push) = serde_json::from_str::<WsPush<FundingRateData>>(&text) {
                                    if push.arg.channel == "funding-rate" {
                                        for data in push.data {
                                            if let Some(rate) = data.to_funding_rate() {
                                                let _ = funding_tx.send(rate);
                                            }
                                        }
                                        continue;
                                    }
                                }

                                // 尝试解析 bbo-tbt
                                if let Ok(push) = serde_json::from_str::<WsPush<BboData>>(&text) {
                                    if push.arg.channel == "bbo-tbt" {
                                        if let Some(inst_id) = &push.arg.inst_id {
                                            for data in push.data {
                                                if let Some(bbo) = data.to_bbo(inst_id) {
                                                    let _ = bbo_tx.send(bbo);
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::warn!("OKX public WebSocket closed");
                                break;
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "OKX WebSocket error");
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

        tracing::info!(url = %WS_PRIVATE_URL, "Connecting to OKX private WebSocket");

        let (ws_stream, _) = connect_async(WS_PRIVATE_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 登录
        let timestamp = OkxRestClient::unix_timestamp();
        let sign = self.rest_client.sign_ws_login(&timestamp);
        let login_msg = json!({
            "op": "login",
            "args": [{
                "apiKey": self.rest_client.api_key(),
                "passphrase": self.rest_client.passphrase(),
                "timestamp": timestamp,
                "sign": sign
            }]
        });

        write
            .send(Message::Text(login_msg.to_string()))
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        // 等待登录响应
        if let Some(Ok(Message::Text(text))) = read.next().await {
            if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                if event.event == "login" && event.code.as_deref() != Some("0") {
                    return Err(ExchangeError::AuthenticationFailed(Exchange::OKX));
                }
            }
        }

        // 订阅私有频道
        let subscribe_msg = json!({
            "op": "subscribe",
            "args": [
                {"channel": "positions", "instType": "SWAP"},
                {"channel": "account"},
                {"channel": "orders", "instType": "SWAP"}
            ]
        });

        write
            .send(Message::Text(subscribe_msg.to_string()))
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

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
                                // 检查是否是事件响应
                                if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                                    if event.event == "subscribe" || event.event == "login" {
                                        continue;
                                    }
                                }

                                // 尝试解析 positions
                                if let Ok(push) = serde_json::from_str::<WsPush<PositionData>>(&text) {
                                    if push.arg.channel == "positions" {
                                        for data in push.data {
                                            if let Some(pos) = data.to_position() {
                                                let _ = position_tx.send(pos);
                                            }
                                        }
                                        continue;
                                    }
                                }

                                // 尝试解析 account
                                if let Ok(push) = serde_json::from_str::<WsPush<AccountData>>(&text) {
                                    if push.arg.channel == "account" {
                                        for acct in push.data {
                                            for detail in acct.details {
                                                if let Some(balance) = detail.to_balance() {
                                                    let _ = balance_tx.send(balance);
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                }

                                // 尝试解析 orders
                                if let Ok(push) = serde_json::from_str::<WsPush<OrderPushData>>(&text) {
                                    if push.arg.channel == "orders" {
                                        for data in push.data {
                                            if let Some(update) = data.to_order_update() {
                                                let _ = order_tx.send(update);
                                            }
                                        }
                                        continue;
                                    }
                                }
                            }
                            Some(Ok(Message::Ping(data))) => {
                                let _ = write.send(Message::Pong(data)).await;
                            }
                            Some(Ok(Message::Close(_))) | None => {
                                tracing::warn!("OKX private WebSocket closed");
                                break;
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "OKX private WebSocket error");
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
impl ExchangeExecutor for OkxWebSocket {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
        self.rest_client.place_order(order).await
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        self.rest_client.set_leverage(symbol, leverage).await
    }
}

use crate::domain::{Exchange, ExchangeError, Symbol};
use crate::exchange::api::{ExchangeWebSocket, PrivateSinks, PublicSinks};
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsEvent, WsPush,
};
use crate::exchange::okx::rest::OkxRestClient;
use crate::exchange::okx::{WS_PRIVATE_URL, WS_PUBLIC_URL};
use crate::exchange::ws_util::{ExponentialBackoff, RetryConfig};
use crate::parse_or_panic;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
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
        sinks: PublicSinks,
        cancel_token: CancellationToken,
    ) -> Result<(), ExchangeError> {
        let symbols = sinks.symbols();
        if symbols.is_empty() {
            return Ok(());
        }

        // 构建订阅消息
        let mut args = Vec::new();
        for symbol in &symbols {
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

        // 启动带重连的消息处理任务
        tokio::spawn(async move {
            let mut backoff = ExponentialBackoff::new(RetryConfig::default());

            loop {
                if cancel_token.is_cancelled() {
                    break;
                }

                tracing::info!(url = %WS_PUBLIC_URL, "Connecting to OKX public WebSocket");

                match connect_async(WS_PUBLIC_URL).await {
                    Ok((ws_stream, _)) => {
                        backoff.reset();
                        let (mut write, mut read) = ws_stream.split();

                        // 发送订阅消息
                        if let Err(e) = write.send(Message::Text(subscribe_msg.to_string())).await {
                            tracing::error!(error = %e, "Failed to send subscribe message");
                            continue;
                        }

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
                                            handle_okx_public_message(
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
                                            tracing::warn!("OKX public WebSocket closed, will reconnect");
                                            break;
                                        }
                                        Some(Err(e)) => {
                                            tracing::error!(error = %e, "OKX WebSocket error, will reconnect");
                                            break;
                                        }
                                        Some(Ok(Message::Pong(_))) => {
                                            // Pong 响应，正常忽略
                                        }
                                        Some(Ok(Message::Binary(data))) => {
                                            panic!("Unexpected binary message from OKX public WebSocket: {} bytes", data.len());
                                        }
                                        Some(Ok(Message::Frame(_))) => {
                                            panic!("Unexpected raw frame from OKX public WebSocket");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to connect to OKX public WebSocket");
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
        let rest_client = self.rest_client.clone();

        // 启动带重连的消息处理任务
        tokio::spawn(async move {
            let mut backoff = ExponentialBackoff::new(RetryConfig::default());

            loop {
                if cancel_token.is_cancelled() {
                    break;
                }

                tracing::info!(url = %WS_PRIVATE_URL, "Connecting to OKX private WebSocket");

                match connect_async(WS_PRIVATE_URL).await {
                    Ok((ws_stream, _)) => {
                        let (mut write, mut read) = ws_stream.split();

                        // 登录
                        let timestamp = OkxRestClient::unix_timestamp();
                        let sign = rest_client.sign_ws_login(&timestamp);
                        let login_msg = json!({
                            "op": "login",
                            "args": [{
                                "apiKey": rest_client.api_key(),
                                "passphrase": rest_client.passphrase(),
                                "timestamp": timestamp,
                                "sign": sign
                            }]
                        });

                        if let Err(e) = write.send(Message::Text(login_msg.to_string())).await {
                            tracing::error!(error = %e, "Failed to send login message");
                            backoff.wait().await;
                            continue;
                        }

                        // 等待登录响应
                        let login_ok = match read.next().await {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<WsEvent>(&text) {
                                    Ok(event) if event.event == "login" => {
                                        if event.code.as_deref() == Some("0") {
                                            tracing::info!("OKX login successful");
                                            true
                                        } else {
                                            tracing::error!(
                                                code = ?event.code,
                                                msg = ?event.msg,
                                                "OKX login failed"
                                            );
                                            false
                                        }
                                    }
                                    Ok(event) => {
                                        tracing::error!(
                                            event = %event.event,
                                            "Unexpected event while waiting for login response"
                                        );
                                        false
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = %e,
                                            raw = %text,
                                            "Failed to parse login response"
                                        );
                                        false
                                    }
                                }
                            }
                            Some(Ok(other)) => {
                                tracing::error!(msg = ?other, "Unexpected message type while waiting for login");
                                false
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "WebSocket error while waiting for login");
                                false
                            }
                            None => {
                                tracing::error!("WebSocket closed while waiting for login");
                                false
                            }
                        };

                        if !login_ok {
                            backoff.wait().await;
                            continue;
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

                        if let Err(e) = write.send(Message::Text(subscribe_msg.to_string())).await {
                            tracing::error!(error = %e, "Failed to send subscribe message");
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
                                            handle_okx_private_message(
                                                &text,
                                                &sinks.positions,
                                                &sinks.balances,
                                                &sinks.order_updates,
                                            );
                                        }
                                        Some(Ok(Message::Ping(data))) => {
                                            if let Err(e) = write.send(Message::Pong(data)).await {
                                                tracing::error!(error = %e, "Failed to send pong");
                                                break;
                                            }
                                        }
                                        Some(Ok(Message::Close(_))) | None => {
                                            tracing::warn!("OKX private WebSocket closed, will reconnect");
                                            break;
                                        }
                                        Some(Err(e)) => {
                                            tracing::error!(error = %e, "OKX private WebSocket error, will reconnect");
                                            break;
                                        }
                                        Some(Ok(Message::Pong(_))) => {
                                            // Pong 响应，正常忽略
                                        }
                                        Some(Ok(Message::Binary(data))) => {
                                            panic!("Unexpected binary message from OKX private WebSocket: {} bytes", data.len());
                                        }
                                        Some(Ok(Message::Frame(_))) => {
                                            panic!("Unexpected raw frame from OKX private WebSocket");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to connect to OKX private WebSocket");
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

/// 处理 OKX 公共消息
///
/// 解析失败时 panic，因为这表示代码逻辑漏洞
fn handle_okx_public_message(
    text: &str,
    funding_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::FundingRate>>,
    bbo_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::BBO>>,
) {
    // 先解析为 Value 判断消息类型
    let value: serde_json::Value = parse_or_panic!(text, serde_json::Value, "OKX public base");

    // 检查是否是事件响应 (subscribe 确认等)
    if let Some(event) = value.get("event").and_then(|e| e.as_str()) {
        match event {
            "subscribe" | "unsubscribe" | "channel-conn-count" => {
                tracing::debug!(event = %event, "OKX event response");
                return;
            }
            "error" => {
                let code = value.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
                panic!("OKX WebSocket error event: code={}, msg={}\nRaw: {}", code, msg, text);
            }
            unknown => {
                panic!("Unknown OKX event type: {}\nRaw: {}", unknown, text);
            }
        }
    }

    // 数据推送消息，通过 arg.channel 判断类型
    let channel = value
        .get("arg")
        .and_then(|arg| arg.get("channel"))
        .and_then(|c| c.as_str());

    match channel {
        Some("funding-rate") => {
            let push: WsPush<FundingRateData> = parse_or_panic!(text, WsPush<FundingRateData>, "funding-rate");
            for data in push.data {
                let rate = data.to_funding_rate();
                // 按 symbol 路由 (未订阅的 symbol 忽略)
                if let Some(tx) = funding_sinks.get(&rate.symbol) {
                    let _ = tx.send(rate);
                }
            }
        }
        Some("bbo-tbt") => {
            let push: WsPush<BboData> = parse_or_panic!(text, WsPush<BboData>, "bbo-tbt");
            let inst_id = push.arg.inst_id.as_ref()
                .unwrap_or_else(|| panic!("bbo-tbt push missing inst_id\nRaw: {}", text));
            for data in push.data {
                let bbo = data.to_bbo(inst_id);
                // 按 symbol 路由 (未订阅的 symbol 忽略)
                if let Some(tx) = bbo_sinks.get(&bbo.symbol) {
                    let _ = tx.send(bbo);
                }
            }
        }
        Some(unknown) => {
            panic!("Unknown OKX public channel: {}\nRaw: {}", unknown, text);
        }
        None => {
            panic!("OKX public message missing channel\nRaw: {}", text);
        }
    }
}

/// 处理 OKX 私有消息
///
/// 解析失败时 panic，因为这表示代码逻辑漏洞
fn handle_okx_private_message(
    text: &str,
    position_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::Position>>,
    balance_sink: &broadcast::Sender<crate::domain::Balance>,
    order_sinks: &HashMap<Symbol, broadcast::Sender<crate::domain::OrderUpdate>>,
) {
    // 先解析为 Value 判断消息类型
    let value: serde_json::Value = parse_or_panic!(text, serde_json::Value, "OKX private base");

    // 检查是否是事件响应
    if let Some(event) = value.get("event").and_then(|e| e.as_str()) {
        match event {
            "subscribe" | "unsubscribe" | "login" | "channel-conn-count" => {
                tracing::debug!(event = %event, "OKX event response");
                return;
            }
            "error" => {
                let code = value.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
                panic!("OKX WebSocket error event: code={}, msg={}\nRaw: {}", code, msg, text);
            }
            unknown => {
                panic!("Unknown OKX event type: {}\nRaw: {}", unknown, text);
            }
        }
    }

    // 数据推送消息
    let channel = value
        .get("arg")
        .and_then(|arg| arg.get("channel"))
        .and_then(|c| c.as_str());

    match channel {
        Some("positions") => {
            let push: WsPush<PositionData> = parse_or_panic!(text, WsPush<PositionData>, "positions");
            for data in push.data {
                let pos = data.to_position();
                // 按 symbol 路由 (未订阅的 symbol 忽略)
                if let Some(tx) = position_sinks.get(&pos.symbol) {
                    let _ = tx.send(pos);
                }
            }
        }
        Some("account") => {
            let push: WsPush<AccountData> = parse_or_panic!(text, WsPush<AccountData>, "account");
            for acct in push.data {
                for detail in acct.details {
                    let balance = detail.to_balance();
                    // 不按 symbol 分
                    let _ = balance_sink.send(balance);
                }
            }
        }
        Some("orders") => {
            let push: WsPush<OrderPushData> = parse_or_panic!(text, WsPush<OrderPushData>, "orders");
            for data in push.data {
                let update = data.to_order_update();
                // 按 symbol 路由 (未订阅的 symbol 忽略)
                if let Some(tx) = order_sinks.get(&update.symbol) {
                    let _ = tx.send(update);
                }
            }
        }
        Some(unknown) => {
            panic!("Unknown OKX private channel: {}\nRaw: {}", unknown, text);
        }
        None => {
            panic!("OKX private message missing channel\nRaw: {}", text);
        }
    }
}

//! OKX 私有 WebSocket 连接
//!
//! 特点：
//! - 连接固定 URL
//! - 连接后发送 login 消息（带签名）
//! - 等待 login 成功响应后才算就绪

use super::private_ws::{run_private_ws_loop, PrivateConnectionHandle};
use super::public_ws::{ConnectionId, WsDataSink, WsError};
use crate::domain::Exchange;
use crate::exchange::okx::{OkxCredentials, WS_PRIVATE_URL};
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use kameo::actor::{ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use serde_json::json;
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

/// OkxPrivateWsActor 初始化参数
pub struct OkxPrivateWsActorArgs<S: WsDataSink> {
    pub conn_id: ConnectionId,
    pub credentials: OkxCredentials,
    pub data_sink: Arc<S>,
}

/// OKX 私有 WebSocket Actor
pub struct OkxPrivateWsActor<S: WsDataSink> {
    conn_id: ConnectionId,
    credentials: OkxCredentials,
    data_sink: Arc<S>,
    write_tx: Option<mpsc::Sender<String>>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl<S: WsDataSink> OkxPrivateWsActor<S> {
    pub fn new(args: OkxPrivateWsActorArgs<S>) -> Self {
        Self {
            conn_id: args.conn_id,
            credentials: args.credentials,
            data_sink: args.data_sink,
            write_tx: None,
            stop_tx: None,
        }
    }
}

impl<S: WsDataSink> Actor for OkxPrivateWsActor<S> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxPrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::debug!(
            exchange = %Exchange::OKX,
            conn_id = self.conn_id.0,
            "OkxPrivateWsActor starting..."
        );

        // 1. 连接
        let ws_stream = connect_async(WS_PRIVATE_URL).await.map_err(|e| {
            WsError::Network(format!("Connect failed: {}", e))
        })?;

        let (mut write, mut read) = ws_stream.0.split();

        // 2. 发送 login 消息
        let login_msg = build_login_message(&self.credentials);
        write.send(WsMessage::Text(login_msg)).await.map_err(|e| {
            WsError::AuthFailed(format!("Login send failed: {}", e))
        })?;

        tracing::debug!(
            exchange = %Exchange::OKX,
            conn_id = self.conn_id.0,
            "Login message sent, waiting for response..."
        );

        // 3. 等待 login 成功响应
        loop {
            match read.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if is_login_success(&text) {
                        tracing::info!(
                            exchange = %Exchange::OKX,
                            conn_id = self.conn_id.0,
                            "OKX login successful"
                        );
                        break;
                    } else if is_login_error(&text) {
                        return Err(WsError::AuthFailed(
                            format!("Login failed: {}", text)
                        ).into());
                    }
                    // 其他消息忽略，继续等待
                }
                Some(Ok(WsMessage::Ping(data))) => {
                    let _ = write.send(WsMessage::Pong(data)).await;
                }
                Some(Ok(WsMessage::Close(_))) | None => {
                    return Err(WsError::AuthFailed(
                        "Connection closed before login success".to_string()
                    ).into());
                }
                Some(Err(e)) => {
                    return Err(WsError::AuthFailed(
                        format!("WebSocket error during login: {}", e)
                    ).into());
                }
                _ => {}
            }
        }

        // 4. 创建通道
        let (write_tx, write_rx) = mpsc::channel::<String>(64);
        let (stop_tx, stop_rx) = oneshot::channel();
        self.write_tx = Some(write_tx);
        self.stop_tx = Some(stop_tx);

        // 5. 启动 ws_loop
        let data_sink = self.data_sink.clone();
        let conn_id = self.conn_id;
        let weak_ref = actor_ref.downgrade();

        tokio::spawn(async move {
            let result = run_private_ws_loop(
                read,
                write,
                write_rx,
                data_sink,
                conn_id,
                Exchange::OKX,
                stop_rx,
            )
            .await;

            if let Err(e) = result {
                tracing::warn!(
                    exchange = %Exchange::OKX,
                    conn_id = conn_id.0,
                    error = %e,
                    "OKX private ws_loop ended with error"
                );
            }

            if let Some(actor_ref) = weak_ref.upgrade() {
                actor_ref.stop_gracefully().await.ok();
            }
        });

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        self.write_tx = None;

        tracing::debug!(
            exchange = %Exchange::OKX,
            conn_id = self.conn_id.0,
            reason = ?reason,
            "OkxPrivateWsActor stopped"
        );
        Ok(())
    }
}

/// 构建 OKX login 消息
fn build_login_message(credentials: &OkxCredentials) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();

    let message = format!("{}GET/users/self/verify", timestamp);
    let mut mac = Hmac::<Sha256>::new_from_slice(credentials.secret.as_bytes())
        .expect("HMAC accepts any size");
    mac.update(message.as_bytes());
    let result = mac.finalize();
    let sign = general_purpose::STANDARD.encode(result.into_bytes());

    json!({
        "op": "login",
        "args": [{
            "apiKey": credentials.api_key,
            "passphrase": credentials.passphrase,
            "timestamp": timestamp,
            "sign": sign
        }]
    })
    .to_string()
}

/// 检查是否是 login 成功响应
fn is_login_success(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|v| {
            v.get("event").and_then(|e| e.as_str()) == Some("login")
                && v.get("code").and_then(|c| c.as_str()) == Some("0")
        })
        .unwrap_or(false)
}

/// 检查是否是 login 错误响应
fn is_login_error(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .map(|v| {
            v.get("event").and_then(|e| e.as_str()) == Some("error")
                || (v.get("event").and_then(|e| e.as_str()) == Some("login")
                    && v.get("code").and_then(|c| c.as_str()) != Some("0"))
        })
        .unwrap_or(false)
}

// === Messages ===

/// 发送消息到 WebSocket
pub struct SendMessage(pub String);

impl<S: WsDataSink> Message<SendMessage> for OkxPrivateWsActor<S> {
    type Reply = bool;

    async fn handle(
        &mut self,
        msg: SendMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(ref tx) = self.write_tx {
            tx.send(msg.0).await.is_ok()
        } else {
            false
        }
    }
}

// === PrivateConnectionHandle 实现 ===

/// OKX 私有连接句柄
pub struct OkxPrivateHandle<S: WsDataSink> {
    actor_ref: ActorRef<OkxPrivateWsActor<S>>,
    conn_id: ConnectionId,
}

impl<S: WsDataSink> OkxPrivateHandle<S> {
    pub fn new(actor_ref: ActorRef<OkxPrivateWsActor<S>>, conn_id: ConnectionId) -> Self {
        Self { actor_ref, conn_id }
    }
}

#[async_trait]
impl<S: WsDataSink> PrivateConnectionHandle for OkxPrivateHandle<S> {
    fn actor_id(&self) -> ActorID {
        self.actor_ref.id()
    }

    fn conn_id(&self) -> ConnectionId {
        self.conn_id
    }

    async fn send_message(&self, msg: String) -> bool {
        self.actor_ref.tell(SendMessage(msg)).await.is_ok()
    }
}

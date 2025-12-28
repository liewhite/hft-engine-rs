//! PublicWsActor - 公共 WebSocket 连接
//!
//! 职责：连接建立、消息收发、错误即死亡
//! 只处理 Public 连接，无认证逻辑

use crate::domain::Exchange;
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

/// 连接 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

/// WebSocket 错误类型
#[derive(Debug, Clone, thiserror::Error)]
pub enum WsError {
    // 可恢复错误
    #[error("network error: {0}")]
    Network(String),
    #[error("connection timeout")]
    Timeout,
    #[error("listen key expired")]
    ListenKeyExpired,
    #[error("connection closed by server")]
    ServerClosed,

    // 不可恢复错误
    #[error("auth failed: {0}")]
    AuthFailed(String),
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

impl WsError {
    /// 判断错误是否可恢复
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            WsError::Network(_) | WsError::Timeout | WsError::ListenKeyExpired | WsError::ServerClosed
        )
    }
}

/// WebSocket 数据消息
#[derive(Debug, Clone)]
pub struct WsData {
    pub conn_id: ConnectionId,
    pub data: String,
}

/// 发送消息的回调 trait (用于类型擦除)
#[async_trait::async_trait]
pub trait WsDataSink: Send + Sync + 'static {
    async fn send_data(&self, data: WsData);
}

/// PublicWsActor 初始化参数
pub struct PublicWsActorArgs<S: WsDataSink> {
    pub conn_id: ConnectionId,
    pub url: String,
    pub exchange: Exchange,
    pub data_sink: Arc<S>,
}

/// PublicWsActor - 公共 WebSocket 连接 Actor
pub struct PublicWsActor<S: WsDataSink> {
    conn_id: ConnectionId,
    url: String,
    exchange: Exchange,
    data_sink: Arc<S>,
    write_tx: Option<mpsc::Sender<String>>,
}

impl<S: WsDataSink> PublicWsActor<S> {
    pub fn new(args: PublicWsActorArgs<S>) -> Self {
        Self {
            conn_id: args.conn_id,
            url: args.url,
            exchange: args.exchange,
            data_sink: args.data_sink,
            write_tx: None,
        }
    }

    pub fn conn_id(&self) -> ConnectionId {
        self.conn_id
    }
}

impl<S: WsDataSink> Actor for PublicWsActor<S> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "PublicWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::debug!(
            exchange = %self.exchange,
            conn_id = self.conn_id.0,
            "PublicWsActor starting, connecting..."
        );

        // 建立 WebSocket 连接
        let ws_stream = connect_async(&self.url).await.map_err(|e| {
            WsError::Network(format!("Connect failed: {}", e))
        })?;

        let (write, read) = ws_stream.0.split();

        tracing::info!(
            exchange = %self.exchange,
            conn_id = self.conn_id.0,
            "Public WebSocket connected"
        );

        // 创建写入通道
        let (write_tx, write_rx) = mpsc::channel::<String>(64);
        self.write_tx = Some(write_tx);

        // 启动 ws_loop 任务
        let weak_ref = actor_ref.downgrade();
        let data_sink = self.data_sink.clone();
        let conn_id = self.conn_id;
        let exchange = self.exchange;

        tokio::spawn(async move {
            run_ws_loop(read, write, write_rx, data_sink, conn_id, exchange, weak_ref).await;
        });

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        self.write_tx = None;

        tracing::debug!(
            exchange = %self.exchange,
            conn_id = self.conn_id.0,
            reason = ?reason,
            "PublicWsActor stopped"
        );
        Ok(())
    }
}

// === Messages ===

/// 发送消息到 WebSocket
pub struct SendMessage(pub String);

impl<S: WsDataSink> Message<SendMessage> for PublicWsActor<S> {
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

// === WebSocket Loop ===

async fn run_ws_loop<S: WsDataSink>(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut write_rx: mpsc::Receiver<String>,
    data_sink: Arc<S>,
    conn_id: ConnectionId,
    exchange: Exchange,
    weak_ref: WeakActorRef<PublicWsActor<S>>,
) {
    loop {
        tokio::select! {
            msg = write_rx.recv() => {
                match msg {
                    Some(text) => {
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            tracing::warn!(
                                exchange = %exchange,
                                conn_id = conn_id.0,
                                "WebSocket send failed, exiting loop"
                            );
                            break;
                        }
                    }
                    None => {
                        let _ = write.close().await;
                        return;
                    }
                }
            }

            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        data_sink.send_data(WsData { conn_id, data: text }).await;
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        tracing::warn!(
                            exchange = %exchange,
                            conn_id = conn_id.0,
                            "WebSocket closed by server"
                        );
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(
                            exchange = %exchange,
                            conn_id = conn_id.0,
                            error = %e,
                            "WebSocket error"
                        );
                        break;
                    }
                    None => {
                        tracing::warn!(
                            exchange = %exchange,
                            conn_id = conn_id.0,
                            "WebSocket stream ended"
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(actor_ref) = weak_ref.upgrade() {
        actor_ref.stop_gracefully().await.ok();
    }
}

//! WebSocketActor - 管理单个 WebSocket 连接
//!
//! 职责：连接建立、消息收发、错误即死亡
//!
//! 生命周期：
//! - on_start: 建立 WebSocket 连接，失败则 Actor die
//! - 内部 tokio::spawn: 处理 ws 读取循环，stream 结束时触发 Actor die
//! - 父 Actor (ExchangeActor) 通过 spawn_link 创建，收到 on_link_died 时重启

use crate::exchange::subscriber::ExchangeConfig;
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

/// 连接类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    Public,
    Private,
}

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

/// WebSocket 数据消息 - 从 WebSocketActor 发送到 ExchangeActor
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

/// WebSocketActor 初始化参数
pub struct WebSocketActorArgs<S: WsDataSink> {
    pub conn_id: ConnectionId,
    pub conn_type: ConnectionType,
    pub url: String,
    /// 数据接收器 (父 Actor 的弱引用)
    pub data_sink: Arc<S>,
}

/// WebSocketActor - 管理单个 WebSocket 连接
pub struct WebSocketActor<C: ExchangeConfig, S: WsDataSink> {
    conn_id: ConnectionId,
    conn_type: ConnectionType,
    url: String,
    credentials: Option<C::Credentials>,
    /// 数据接收器 (父 Actor)
    data_sink: Arc<S>,
    /// WebSocket 写端
    write_tx: Option<mpsc::Sender<String>>,
    _marker: std::marker::PhantomData<C>,
}

impl<C: ExchangeConfig, S: WsDataSink> WebSocketActor<C, S> {
    pub fn new(args: WebSocketActorArgs<S>, credentials: Option<C::Credentials>) -> Self {
        Self {
            conn_id: args.conn_id,
            conn_type: args.conn_type,
            url: args.url,
            credentials,
            data_sink: args.data_sink,
            write_tx: None,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn conn_id(&self) -> ConnectionId {
        self.conn_id
    }
}

impl<C: ExchangeConfig, S: WsDataSink> Actor for WebSocketActor<C, S> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "WebSocketActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::debug!(
            exchange = %C::EXCHANGE,
            conn_id = self.conn_id.0,
            conn_type = ?self.conn_type,
            "WebSocketActor starting, connecting..."
        );

        // 建立 WebSocket 连接
        let ws_stream = connect_async(&self.url).await.map_err(|e| {
            WsError::Network(format!("Connect failed: {}", e))
        })?;

        let (mut write, read) = ws_stream.0.split();

        // Private 连接需要认证
        if self.conn_type == ConnectionType::Private {
            if let Some(ref credentials) = self.credentials {
                let auth_msg = C::build_auth_msg(credentials);
                if !auth_msg.is_empty() {
                    write.send(WsMessage::Text(auth_msg)).await.map_err(|e| {
                        WsError::AuthFailed(format!("Auth send failed: {}", e))
                    })?;
                }
            }
        }

        tracing::info!(
            exchange = %C::EXCHANGE,
            conn_id = self.conn_id.0,
            conn_type = ?self.conn_type,
            "WebSocket connected"
        );

        // 创建写入通道
        let (write_tx, mut write_rx) = mpsc::channel::<String>(64);
        self.write_tx = Some(write_tx);

        // 启动内部 ws_loop 任务
        let weak_ref = actor_ref.downgrade();
        let data_sink = self.data_sink.clone();
        let conn_id = self.conn_id;

        tokio::spawn(async move {
            run_ws_loop::<C, S>(read, write, &mut write_rx, data_sink, conn_id, weak_ref).await;
        });

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // 关闭写入通道，触发 ws_loop 退出
        self.write_tx = None;

        tracing::debug!(
            exchange = %C::EXCHANGE,
            conn_id = self.conn_id.0,
            reason = ?reason,
            "WebSocketActor stopped"
        );
        Ok(())
    }
}

// === Messages ===

/// 发送消息到 WebSocket
pub struct SendMessage(pub String);

impl<C: ExchangeConfig, S: WsDataSink> Message<SendMessage> for WebSocketActor<C, S> {
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

// === WebSocket 连接循环 ===

async fn run_ws_loop<C: ExchangeConfig, S: WsDataSink>(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    write_rx: &mut mpsc::Receiver<String>,
    data_sink: Arc<S>,
    conn_id: ConnectionId,
    weak_ref: WeakActorRef<WebSocketActor<C, S>>,
) {
    loop {
        tokio::select! {
            // 发送请求
            msg = write_rx.recv() => {
                match msg {
                    Some(text) => {
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            tracing::warn!(
                                exchange = %C::EXCHANGE,
                                conn_id = conn_id.0,
                                "WebSocket send failed, exiting loop"
                            );
                            break;
                        }
                    }
                    None => {
                        // write_rx 关闭 (Actor 停止)，正常退出
                        let _ = write.close().await;
                        return; // 不触发 stop，因为 Actor 已经在停止
                    }
                }
            }

            // 接收 WebSocket 消息
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
                            exchange = %C::EXCHANGE,
                            conn_id = conn_id.0,
                            "WebSocket closed by server"
                        );
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::warn!(
                            exchange = %C::EXCHANGE,
                            conn_id = conn_id.0,
                            error = %e,
                            "WebSocket error"
                        );
                        break;
                    }
                    None => {
                        tracing::warn!(
                            exchange = %C::EXCHANGE,
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

    // stream 结束或错误，触发 Actor die
    if let Some(actor_ref) = weak_ref.upgrade() {
        actor_ref.stop_gracefully().await.ok();
    }
}

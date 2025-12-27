//! WebSocketActor - 管理单个 WebSocket 连接
//!
//! 职责：连接建立、消息收发、断线检测

use crate::exchange::subscriber::ExchangeConfig;
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
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

/// 上游事件 - 从 WebSocketActor 发送到 ExchangeActor
#[derive(Debug)]
pub enum UpstreamEvent {
    /// 收到数据
    Data { conn_id: ConnectionId, data: String },
    /// 连接断开
    Disconnected {
        conn_id: ConnectionId,
        error: Option<String>,
    },
}

/// WebSocketActor 初始化参数
pub struct WebSocketActorArgs<C: ExchangeConfig> {
    pub conn_id: ConnectionId,
    pub conn_type: ConnectionType,
    pub url: String,
    pub credentials: C::Credentials,
    /// 上游事件通道 (发送到 ExchangeActor)
    pub upstream: mpsc::Sender<UpstreamEvent>,
}

/// WebSocketActor - 管理单个 WebSocket 连接
pub struct WebSocketActor<C: ExchangeConfig> {
    conn_id: ConnectionId,
    conn_type: ConnectionType,
    url: String,
    credentials: C::Credentials,
    /// 上游事件通道
    upstream: mpsc::Sender<UpstreamEvent>,
    /// WebSocket 写端
    write_tx: Option<mpsc::Sender<String>>,
    /// WebSocket 任务句柄
    ws_task: Option<tokio::task::JoinHandle<()>>,
    _marker: std::marker::PhantomData<C>,
}

impl<C: ExchangeConfig> WebSocketActor<C> {
    pub fn new(args: WebSocketActorArgs<C>) -> Self {
        Self {
            conn_id: args.conn_id,
            conn_type: args.conn_type,
            url: args.url,
            credentials: args.credentials,
            upstream: args.upstream,
            write_tx: None,
            ws_task: None,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn conn_id(&self) -> ConnectionId {
        self.conn_id
    }

    /// 停止 WebSocket 任务
    fn stop_ws_task(&mut self) {
        self.write_tx = None;
        if let Some(handle) = self.ws_task.take() {
            handle.abort();
        }
    }
}

impl<C: ExchangeConfig> Actor for WebSocketActor<C> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "WebSocketActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::debug!(
            exchange = %C::EXCHANGE,
            conn_id = self.conn_id.0,
            conn_type = ?self.conn_type,
            "WebSocketActor started"
        );
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        self.stop_ws_task();
        tracing::debug!(
            exchange = %C::EXCHANGE,
            conn_id = self.conn_id.0,
            "WebSocketActor stopped"
        );
        Ok(())
    }
}

// === Messages ===

/// 建立连接
pub struct Connect;

impl<C: ExchangeConfig> Message<Connect> for WebSocketActor<C> {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Connect,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 如果已有连接，先停止
        self.stop_ws_task();

        let (write_tx, write_rx) = mpsc::channel::<String>(64);

        let url = self.url.clone();
        let credentials = self.credentials.clone();
        let is_private = self.conn_type == ConnectionType::Private;
        let conn_id = self.conn_id;
        let upstream = self.upstream.clone();

        // 启动连接任务并保存 JoinHandle
        let handle = tokio::spawn(async move {
            run_ws_loop::<C>(url, credentials, is_private, write_rx, upstream, conn_id).await;
        });

        self.write_tx = Some(write_tx);
        self.ws_task = Some(handle);
    }
}

/// 发送消息到 WebSocket
pub struct SendMessage(pub String);

impl<C: ExchangeConfig> Message<SendMessage> for WebSocketActor<C> {
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

async fn run_ws_loop<C: ExchangeConfig>(
    url: String,
    credentials: C::Credentials,
    is_private: bool,
    mut write_rx: mpsc::Receiver<String>,
    upstream: mpsc::Sender<UpstreamEvent>,
    conn_id: ConnectionId,
) {
    // 连接 WebSocket
    let ws_stream = match connect_async(&url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            let _ = upstream
                .send(UpstreamEvent::Disconnected {
                    conn_id,
                    error: Some(format!("Connect failed: {}", e)),
                })
                .await;
            return;
        }
    };

    let (mut write, mut read) = ws_stream.split();

    // Private 连接需要认证
    if is_private {
        let auth_msg = C::build_auth_msg(&credentials);
        if !auth_msg.is_empty() {
            if let Err(e) = write.send(WsMessage::Text(auth_msg)).await {
                let _ = upstream
                    .send(UpstreamEvent::Disconnected {
                        conn_id,
                        error: Some(format!("Auth send failed: {}", e)),
                    })
                    .await;
                return;
            }
        }
    }

    tracing::info!(
        exchange = %C::EXCHANGE,
        conn_id = conn_id.0,
        is_private = is_private,
        "WebSocket connected"
    );

    // 消息循环
    loop {
        tokio::select! {
            // 发送请求
            msg = write_rx.recv() => {
                match msg {
                    Some(text) => {
                        if let Err(e) = write.send(WsMessage::Text(text)).await {
                            let _ = upstream.send(UpstreamEvent::Disconnected {
                                conn_id,
                                error: Some(format!("Send failed: {}", e)),
                            }).await;
                            return;
                        }
                    }
                    None => {
                        // write_rx 关闭，正常退出
                        let _ = write.close().await;
                        return;
                    }
                }
            }

            // 接收 WebSocket 消息
            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        // 如果 upstream 关闭，退出循环
                        if upstream.send(UpstreamEvent::Data {
                            conn_id,
                            data: text,
                        }).await.is_err() {
                            tracing::debug!(
                                exchange = %C::EXCHANGE,
                                conn_id = conn_id.0,
                                "Upstream channel closed, exiting ws loop"
                            );
                            return;
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        let _ = upstream.send(UpstreamEvent::Disconnected {
                            conn_id,
                            error: Some("WebSocket closed by server".into()),
                        }).await;
                        return;
                    }
                    Some(Err(e)) => {
                        let _ = upstream.send(UpstreamEvent::Disconnected {
                            conn_id,
                            error: Some(format!("WebSocket error: {}", e)),
                        }).await;
                        return;
                    }
                    None => {
                        let _ = upstream.send(UpstreamEvent::Disconnected {
                            conn_id,
                            error: Some("WebSocket stream ended".into()),
                        }).await;
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}

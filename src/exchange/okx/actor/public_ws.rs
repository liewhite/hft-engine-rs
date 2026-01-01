//! OkxPublicWsActor - 管理 OKX 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 将收到的消息转发给父 OkxActor

use super::okx_actor::OkxActor;
use crate::exchange::client::{Subscribe, SubscriptionKind, Unsubscribe, WsError};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use std::collections::HashSet;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";

/// 协程退出信号
#[derive(Debug)]
enum WsExit {
    Error(WsError),
}

/// OkxPublicWsActor 初始化参数
pub struct OkxPublicWsActorArgs {
    /// 父 Actor 弱引用
    pub parent: WeakActorRef<OkxActor>,
}

/// OkxPublicWsActor - 公开 WebSocket Actor
pub struct OkxPublicWsActor {
    /// 父 Actor 弱引用
    parent: WeakActorRef<OkxActor>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,
}

impl OkxPublicWsActor {
    /// 创建新的 OkxPublicWsActor
    pub fn new(args: OkxPublicWsActorArgs) -> Self {
        Self {
            parent: args.parent,
            ws_tx: None,
            subscribed: HashSet::new(),
        }
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind);
        let msg = json!({
            "op": "subscribe",
            "args": [arg]
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind);
        let msg = json!({
            "op": "unsubscribe",
            "args": [arg]
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }
}

impl Actor for OkxPublicWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxPublicWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 创建消息发送 channel
        let (ws_tx, ws_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(ws_tx);

        // 创建退出信号 channel
        let (exit_tx, exit_rx) = mpsc::channel::<WsExit>(1);

        // attach_stream 监控退出信号
        let exit_stream = ReceiverStream::new(exit_rx);
        actor_ref.attach_stream(exit_stream, (), ());

        // 启动 ws_loop
        let weak_ref = actor_ref.downgrade();
        tokio::spawn(run_ws_loop(read, write, ws_rx, weak_ref, exit_tx));

        tracing::info!("OkxPublicWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        self.ws_tx.take();
        tracing::info!("OkxPublicWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if self.subscribed.contains(&msg.kind) {
            return;
        }

        if let Err(e) = self.send_subscribe(&msg.kind).await {
            tracing::error!(error = %e, "Failed to send subscribe, killing actor");
            ctx.actor_ref().kill();
            return;
        }
        self.subscribed.insert(msg.kind);
    }
}

impl Message<Unsubscribe> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.subscribed.remove(&msg.kind) {
            return;
        }

        if let Err(e) = self.send_unsubscribe(&msg.kind).await {
            tracing::error!(error = %e, "Failed to send unsubscribe, killing actor");
            ctx.actor_ref().kill();
        }
    }
}

/// WebSocket 数据消息 (从 ws_loop 收到)
pub struct WsData {
    pub data: String,
}

impl Message<WsData> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        if let Some(parent) = self.parent.upgrade() {
            parent
                .tell(super::WsData { data: msg.data })
                .await
                .expect("Failed to forward WsData to parent");
        }
    }
}

/// 协程退出信号处理
impl Message<StreamMessage<WsExit, (), ()>> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<WsExit, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(WsExit::Error(e)) => {
                tracing::error!(error = %e, "Public WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsExit stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::debug!("WsExit stream finished");
            }
        }
    }
}

// ============================================================================
// WebSocket 循环
// ============================================================================

async fn run_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut rx: mpsc::Receiver<String>,
    actor_ref: WeakActorRef<OkxPublicWsActor>,
    exit_tx: mpsc::Sender<WsExit>,
) {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut rx, &actor_ref).await;

    if let Err(e) = result {
        let _ = exit_tx.send(WsExit::Error(e)).await;
    }
}

async fn run_ws_loop_inner(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin
              + Send),
    write: &mut (impl SinkExt<WsMessage> + Unpin + Send),
    rx: &mut mpsc::Receiver<String>,
    actor_ref: &WeakActorRef<OkxPublicWsActor>,
) -> Result<(), WsError> {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(text) => {
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            return Err(WsError::Network("Send failed".to_string()));
                        }
                    }
                    None => return Ok(()),
                }
            }

            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        let Some(actor) = actor_ref.upgrade() else {
                            return Ok(());
                        };
                        actor.tell(WsData { data: text }).await.map_err(|e| {
                            WsError::Network(format!("Failed to tell actor: {}", e))
                        })?;
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        write.send(WsMessage::Pong(data)).await.map_err(|_| {
                            WsError::Network("Failed to send pong".to_string())
                        })?;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        return Err(WsError::ServerClosed);
                    }
                    Some(Err(e)) => {
                        return Err(WsError::Network(e.to_string()));
                    }
                    None => {
                        return Err(WsError::ServerClosed);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

fn kind_to_arg(kind: &SubscriptionKind) -> serde_json::Value {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            json!({
                "channel": "funding-rate",
                "instId": symbol.to_okx()
            })
        }
        SubscriptionKind::BBO { symbol } => {
            json!({
                "channel": "bbo-tbt",
                "instId": symbol.to_okx()
            })
        }
    }
}

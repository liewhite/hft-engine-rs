//! OkxPrivateWsActor - 管理 OKX 私有 WebSocket 连接
//!
//! 职责:
//! - 建立私有 WebSocket 连接并完成登录
//! - 自动订阅私有频道 (positions, account, orders)
//! - 将收到的私有消息转发给父 OkxActor

use super::okx_actor::OkxActor;
use crate::exchange::client::WsError;
use crate::exchange::okx::codec::WsEvent;
use crate::exchange::okx::OkxCredentials;
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Private WebSocket URL
const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";

/// 协程退出信号
#[derive(Debug)]
enum WsExit {
    Error(WsError),
}

/// OkxPrivateWsActor 初始化参数
pub struct OkxPrivateWsActorArgs {
    /// 父 Actor 弱引用
    pub parent: WeakActorRef<OkxActor>,
    /// 凭证
    pub credentials: OkxCredentials,
}

/// OkxPrivateWsActor - 私有 WebSocket Actor
pub struct OkxPrivateWsActor {
    /// 父 Actor 弱引用
    parent: WeakActorRef<OkxActor>,
    /// 凭证
    credentials: OkxCredentials,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
}

impl OkxPrivateWsActor {
    /// 创建新的 OkxPrivateWsActor
    pub fn new(args: OkxPrivateWsActorArgs) -> Self {
        Self {
            parent: args.parent,
            credentials: args.credentials,
            ws_tx: None,
        }
    }
}

impl Actor for OkxPrivateWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxPrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 1. 连接私有 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PRIVATE_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 2. 发送 login 消息
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let sign = self.credentials.sign_ws_login(&timestamp);

        let login_msg = json!({
            "op": "login",
            "args": [{
                "apiKey": self.credentials.api_key,
                "passphrase": self.credentials.passphrase,
                "timestamp": timestamp,
                "sign": sign
            }]
        })
        .to_string();

        write
            .send(WsMessage::Text(login_msg))
            .await
            .map_err(|e| WsError::AuthFailed(e.to_string()))?;

        // 3. 等待 login 响应
        loop {
            match read.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                        if event.event == "login" {
                            if event.code.as_deref() == Some("0") {
                                tracing::info!("OKX private login success");
                                break;
                            } else {
                                return Err(WsError::AuthFailed(format!(
                                    "Login failed: {:?}",
                                    event.msg
                                ))
                                .into());
                            }
                        }
                    }
                }
                Some(Ok(WsMessage::Ping(data))) => {
                    write
                        .send(WsMessage::Pong(data))
                        .await
                        .map_err(|e| WsError::Network(format!("Failed to send pong: {}", e)))?;
                }
                Some(Err(e)) => {
                    return Err(WsError::Network(e.to_string()).into());
                }
                None => {
                    return Err(WsError::ServerClosed.into());
                }
                _ => {}
            }
        }

        // 4. 订阅私有频道
        let subscribe_msg = json!({
            "op": "subscribe",
            "args": [
                {"channel": "positions", "instType": "SWAP"},
                {"channel": "account"},
                {"channel": "orders", "instType": "SWAP"}
            ]
        })
        .to_string();

        write
            .send(WsMessage::Text(subscribe_msg))
            .await
            .map_err(|e| WsError::Network(e.to_string()))?;

        // 5. 创建消息发送 channel
        let (ws_tx, ws_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(ws_tx);

        // 6. 创建退出信号 channel
        let (exit_tx, exit_rx) = mpsc::channel::<WsExit>(1);

        // attach_stream 监控退出信号
        let exit_stream = ReceiverStream::new(exit_rx);
        actor_ref.attach_stream(exit_stream, (), ());

        // 7. 启动 ws_loop
        let weak_ref = actor_ref.downgrade();
        tokio::spawn(run_ws_loop(read, write, ws_rx, weak_ref, exit_tx));

        tracing::info!("OkxPrivateWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        self.ws_tx.take();
        tracing::info!("OkxPrivateWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 数据消息 (从 ws_loop 收到)
pub struct WsData {
    pub data: String,
}

impl Message<WsData> for OkxPrivateWsActor {
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
impl Message<StreamMessage<WsExit, (), ()>> for OkxPrivateWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<WsExit, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(WsExit::Error(e)) => {
                tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
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
    actor_ref: WeakActorRef<OkxPrivateWsActor>,
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
    actor_ref: &WeakActorRef<OkxPrivateWsActor>,
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

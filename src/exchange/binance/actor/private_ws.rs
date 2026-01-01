//! BinancePrivateWsActor - 管理 Binance 私有 WebSocket 连接
//!
//! 职责:
//! - 获取 ListenKey 并建立私有 WebSocket 连接
//! - 管理 BinanceListenKeyActor 子 actor (定时刷新 ListenKey)
//! - 将收到的私有消息转发给父 BinanceActor

use super::listen_key::{BinanceListenKeyActor, BinanceListenKeyActorArgs};
use super::binance_actor::BinanceActor;
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::WsError;
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Private WebSocket URL 基础
const WS_PRIVATE_BASE: &str = "wss://fstream.binance.com/ws";

/// 协程退出信号
#[derive(Debug)]
enum WsExit {
    Error(WsError),
}

/// BinancePrivateWsActor 初始化参数
pub struct BinancePrivateWsActorArgs {
    /// 父 Actor 弱引用
    pub parent: WeakActorRef<BinanceActor>,
    /// 凭证
    pub credentials: BinanceCredentials,
    /// REST API 基础 URL
    pub rest_base_url: String,
}

/// BinancePrivateWsActor - 私有 WebSocket Actor
pub struct BinancePrivateWsActor {
    /// 父 Actor 弱引用
    parent: WeakActorRef<BinanceActor>,
    /// 凭证
    credentials: BinanceCredentials,
    /// REST API 基础 URL
    rest_base_url: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// ListenKey 子 actor
    listen_key_actor: Option<ActorRef<BinanceListenKeyActor>>,
    /// 子 actor ID 映射
    listen_key_actor_id: Option<ActorID>,
}

impl BinancePrivateWsActor {
    /// 创建新的 BinancePrivateWsActor
    pub fn new(args: BinancePrivateWsActorArgs) -> Self {
        Self {
            parent: args.parent,
            credentials: args.credentials,
            rest_base_url: args.rest_base_url,
            ws_tx: None,
            listen_key_actor: None,
            listen_key_actor_id: None,
        }
    }
}

impl Actor for BinancePrivateWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinancePrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 1. 获取 ListenKey
        let listen_key = create_listen_key(&self.rest_base_url, &self.credentials.api_key).await?;

        // 2. 连接私有 WebSocket
        let url = format!("{}/{}", WS_PRIVATE_BASE, listen_key);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 3. 创建消息发送 channel
        let (ws_tx, ws_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(ws_tx);

        // 4. 创建退出信号 channel
        let (exit_tx, exit_rx) = mpsc::channel::<WsExit>(1);

        // attach_stream 监控退出信号
        let exit_stream = ReceiverStream::new(exit_rx);
        actor_ref.attach_stream(exit_stream, (), ());

        // 5. 启动 ws_loop
        let weak_ref = actor_ref.downgrade();
        tokio::spawn(run_ws_loop(read, write, ws_rx, weak_ref, exit_tx));

        // 6. spawn_link ListenKeyActor
        let listen_key_actor = spawn_link(
            &actor_ref,
            BinanceListenKeyActor::new(BinanceListenKeyActorArgs {
                rest_base_url: self.rest_base_url.clone(),
                api_key: self.credentials.api_key.clone(),
            }),
        )
        .await;
        self.listen_key_actor_id = Some(listen_key_actor.id());
        self.listen_key_actor = Some(listen_key_actor);

        tracing::info!("BinancePrivateWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // Drop ws_tx 会导致 ws_loop 退出
        self.ws_tx.take();
        tracing::info!("BinancePrivateWsActor stopped");
        Ok(())
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorID,
        reason: ActorStopReason,
    ) -> Result<Option<ActorStopReason>, BoxError> {
        // ListenKeyActor 死亡 -> 级联退出
        if Some(id) == self.listen_key_actor_id {
            tracing::error!(reason = ?reason, "BinanceListenKeyActor died, shutting down");
            self.listen_key_actor = None;
            self.listen_key_actor_id = None;
            return Ok(Some(ActorStopReason::LinkDied {
                id,
                reason: Box::new(reason),
            }));
        }

        tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
        Ok(None)
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 数据消息 (从 ws_loop 收到)
pub struct WsData {
    pub data: String,
}

impl Message<WsData> for BinancePrivateWsActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        // 转发给父 Actor
        if let Some(parent) = self.parent.upgrade() {
            parent
                .tell(super::WsData { data: msg.data })
                .await
                .expect("Failed to forward WsData to parent");
        }
    }
}

/// 协程退出信号处理
impl Message<StreamMessage<WsExit, (), ()>> for BinancePrivateWsActor {
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
    actor_ref: WeakActorRef<BinancePrivateWsActor>,
    exit_tx: mpsc::Sender<WsExit>,
) {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut rx, &actor_ref).await;

    // 出错时发送退出信号
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
    actor_ref: &WeakActorRef<BinancePrivateWsActor>,
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

async fn create_listen_key(rest_base_url: &str, api_key: &str) -> Result<String, WsError> {
    #[derive(serde::Deserialize)]
    struct Response {
        #[serde(rename = "listenKey")]
        listen_key: String,
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/fapi/v1/listenKey", rest_base_url))
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await
        .map_err(|e| WsError::AuthFailed(e.to_string()))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(WsError::AuthFailed(format!(
            "Failed to create listen key: {}",
            text
        )));
    }

    let data: Response = resp
        .json()
        .await
        .map_err(|e| WsError::AuthFailed(e.to_string()))?;

    Ok(data.listen_key)
}

//! BinancePrivateWsActor - 管理 Binance 私有 WebSocket 连接
//!
//! 职责:
//! - 获取 ListenKey 并建立私有 WebSocket 连接
//! - 管理 BinanceListenKeyActor 子 actor (定时刷新 ListenKey)
//! - 将收到的私有消息转发给父 BinanceActor

use super::binance_actor::BinanceActor;
use super::listen_key::{BinanceListenKeyActor, BinanceListenKeyActorArgs};
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::WsError;
use crate::exchange::ws_loop::{self, WsIncoming};
use futures_util::StreamExt;
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Private WebSocket URL 基础
const WS_PRIVATE_BASE: &str = "wss://fstream.binance.com/ws";

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

        // 3. 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(outgoing_tx);

        // 4. 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<WsIncoming>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 5. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

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

/// WebSocket 入站消息处理
impl Message<StreamMessage<WsIncoming, (), ()>> for BinancePrivateWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<WsIncoming, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(incoming) => match incoming {
                WsIncoming::Data(data) => {
                    // 转发给父 Actor
                    let parent = self
                        .parent
                        .upgrade()
                        .expect("Parent actor must be alive while child is running");
                    parent
                        .tell(super::WsData { data })
                        .await
                        .expect("Failed to forward WsData to parent");
                }
                WsIncoming::Error(e) => {
                    tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
                    ctx.actor_ref().kill();
                }
            },
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 正常退出（outgoing_tx 被 drop）
                tracing::debug!("WsIncoming stream finished");
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

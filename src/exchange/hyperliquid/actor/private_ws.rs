//! HyperliquidPrivateWsActor - 管理 Hyperliquid 账户 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 自动订阅账户频道 (webData3, orderUpdates)
//! - 将收到的消息转发给父 HyperliquidActor
//!
//! 注意: Hyperliquid 的账户订阅不需要认证，只需要用户地址

use super::hyperliquid_actor::HyperliquidActor;
use super::WsData;
use crate::exchange::client::WsError;
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// HyperliquidPrivateWsActor 初始化参数
pub struct HyperliquidPrivateWsActorArgs {
    /// 父 Actor 弱引用
    pub parent: WeakActorRef<HyperliquidActor>,
    /// 用户钱包地址 (0x...)
    pub wallet_address: String,
}

/// HyperliquidPrivateWsActor - 账户 WebSocket Actor
pub struct HyperliquidPrivateWsActor {
    /// 父 Actor 弱引用
    parent: WeakActorRef<HyperliquidActor>,
    /// 用户钱包地址
    wallet_address: String,
    /// 发送消息到 ws_loop 的 channel
    #[allow(dead_code)]
    ws_tx: Option<mpsc::Sender<String>>,
}

impl HyperliquidPrivateWsActor {
    /// 创建新的 HyperliquidPrivateWsActor
    pub fn new(args: HyperliquidPrivateWsActorArgs) -> Self {
        Self {
            parent: args.parent,
            wallet_address: args.wallet_address,
            ws_tx: None,
        }
    }
}

impl Actor for HyperliquidPrivateWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "HyperliquidPrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 1. 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 2. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(outgoing_tx.clone());

        // 3. 创建入站消息 channel
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 4. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        // 5. 订阅账户频道
        // webData3: 账户状态 (positions, balance)
        let subscribe_web_data = json!({
            "method": "subscribe",
            "subscription": {
                "type": "webData3",
                "user": self.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_web_data)
            .await
            .map_err(|_| WsError::Network("Failed to send webData3 subscription".to_string()))?;

        // orderUpdates: 订单更新
        let subscribe_orders = json!({
            "method": "subscribe",
            "subscription": {
                "type": "orderUpdates",
                "user": self.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_orders)
            .await
            .map_err(|_| WsError::Network("Failed to send orderUpdates subscription".to_string()))?;

        tracing::info!(
            wallet = %self.wallet_address,
            "HyperliquidPrivateWsActor started, subscribed to webData3 and orderUpdates"
        );

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        self.ws_tx.take();
        tracing::info!("HyperliquidPrivateWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for HyperliquidPrivateWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Result<String, WsError>, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                // 转发给父 Actor
                let parent = self
                    .parent
                    .upgrade()
                    .expect("Parent actor must be alive while child is running");
                parent
                    .tell(WsData { data })
                    .await
                    .expect("Failed to forward WsData to parent");
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Private WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::debug!("Private WsIncoming stream finished");
            }
        }
    }
}

//! HyperliquidPublicWsActor - 管理 Hyperliquid WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 将收到的消息转发给父 HyperliquidActor

use super::hyperliquid_actor::HyperliquidActor;
use super::WsData;
use crate::exchange::client::{Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use std::collections::HashSet;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// HyperliquidPublicWsActor 初始化参数
pub struct HyperliquidPublicWsActorArgs {
    /// 父 Actor 弱引用
    pub parent: WeakActorRef<HyperliquidActor>,
}

/// HyperliquidPublicWsActor - WebSocket Actor
pub struct HyperliquidPublicWsActor {
    /// 父 Actor 弱引用
    parent: WeakActorRef<HyperliquidActor>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,
}

impl HyperliquidPublicWsActor {
    /// 创建新的 HyperliquidPublicWsActor
    pub fn new(args: HyperliquidPublicWsActorArgs) -> Self {
        Self {
            parent: args.parent,
            ws_tx: None,
            subscribed: HashSet::new(),
        }
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let subscription = kind_to_subscription(kind);
        let msg = json!({
            "method": "subscribe",
            "subscription": subscription
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let subscription = kind_to_subscription(kind);
        let msg = json!({
            "method": "unsubscribe",
            "subscription": subscription
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }
}

impl Actor for HyperliquidPublicWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "HyperliquidPublicWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(outgoing_tx);

        // 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        tracing::info!("HyperliquidPublicWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // Drop ws_tx 会导致 ws_loop 退出
        self.ws_tx.take();
        tracing::info!("HyperliquidPublicWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for HyperliquidPublicWsActor {
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

impl Message<Unsubscribe> for HyperliquidPublicWsActor {
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

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for HyperliquidPublicWsActor {
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
                tracing::error!(error = %e, "WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 正常退出
                tracing::debug!("WsIncoming stream finished");
            }
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 将 SubscriptionKind 转换为 Hyperliquid 订阅格式
fn kind_to_subscription(kind: &SubscriptionKind) -> serde_json::Value {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            // 使用 activeAssetCtx 获取资金费率
            json!({
                "type": "activeAssetCtx",
                "coin": symbol.to_hyperliquid()
            })
        }
        SubscriptionKind::BBO { symbol } => {
            // 使用 bbo 获取最优买卖价
            json!({
                "type": "bbo",
                "coin": symbol.to_hyperliquid()
            })
        }
    }
}

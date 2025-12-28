//! Binance 私有 WebSocket 连接
//!
//! 特点：
//! - 通过 REST API 获取 ListenKey
//! - URL 中包含 ListenKey，无需发送认证消息
//! - 需要每 30 分钟续期 ListenKey

use super::private_ws::{run_private_ws_loop, PrivateConnectionHandle};
use super::public_ws::{ConnectionId, WsDataSink, WsError};
use crate::domain::Exchange;
use crate::exchange::binance::BinanceRestClient;
use async_trait::async_trait;
use futures_util::StreamExt;
use kameo::actor::{ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::connect_async;

/// ListenKey 续期间隔 (30 分钟)
const LISTEN_KEY_RENEWAL_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Binance 私有 WebSocket URL 基础
const BINANCE_PRIVATE_WS_BASE: &str = "wss://fstream.binance.com/ws";

/// BinancePrivateWsActor 初始化参数
pub struct BinancePrivateWsActorArgs<S: WsDataSink> {
    pub conn_id: ConnectionId,
    pub rest_client: Arc<BinanceRestClient>,
    pub data_sink: Arc<S>,
}

/// Binance 私有 WebSocket Actor
pub struct BinancePrivateWsActor<S: WsDataSink> {
    conn_id: ConnectionId,
    rest_client: Arc<BinanceRestClient>,
    data_sink: Arc<S>,
    write_tx: Option<mpsc::Sender<String>>,
    stop_tx: Option<oneshot::Sender<()>>,
}

impl<S: WsDataSink> BinancePrivateWsActor<S> {
    pub fn new(args: BinancePrivateWsActorArgs<S>) -> Self {
        Self {
            conn_id: args.conn_id,
            rest_client: args.rest_client,
            data_sink: args.data_sink,
            write_tx: None,
            stop_tx: None,
        }
    }
}

impl<S: WsDataSink> Actor for BinancePrivateWsActor<S> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinancePrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::debug!(
            exchange = %Exchange::Binance,
            conn_id = self.conn_id.0,
            "BinancePrivateWsActor starting..."
        );

        // 1. 获取 ListenKey
        let listen_key = self.rest_client.create_listen_key().await.map_err(|e| {
            WsError::AuthFailed(format!("Failed to get ListenKey: {}", e))
        })?;

        tracing::info!(
            exchange = %Exchange::Binance,
            conn_id = self.conn_id.0,
            "ListenKey obtained"
        );

        // 2. 构建 URL 并连接
        let url = format!("{}/{}", BINANCE_PRIVATE_WS_BASE, listen_key);
        let ws_stream = connect_async(&url).await.map_err(|e| {
            WsError::Network(format!("Connect failed: {}", e))
        })?;

        let (write, read) = ws_stream.0.split();

        tracing::info!(
            exchange = %Exchange::Binance,
            conn_id = self.conn_id.0,
            "Binance private WebSocket connected"
        );

        // 3. 创建通道
        let (write_tx, write_rx) = mpsc::channel::<String>(64);
        let (stop_tx, stop_rx) = oneshot::channel();
        self.write_tx = Some(write_tx);
        self.stop_tx = Some(stop_tx);

        // 4. 启动 ListenKey 续期定时器
        let rest_client = self.rest_client.clone();
        let weak_ref = actor_ref.downgrade();
        tokio::spawn(async move {
            listen_key_renewal_loop(rest_client, weak_ref).await;
        });

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
                Exchange::Binance,
                stop_rx,
            )
            .await;

            // ws_loop 结束，触发 Actor die
            if let Err(e) = result {
                tracing::warn!(
                    exchange = %Exchange::Binance,
                    conn_id = conn_id.0,
                    error = %e,
                    "Binance private ws_loop ended with error"
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
        // 发送停止信号
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        self.write_tx = None;

        tracing::debug!(
            exchange = %Exchange::Binance,
            conn_id = self.conn_id.0,
            reason = ?reason,
            "BinancePrivateWsActor stopped"
        );
        Ok(())
    }
}

/// ListenKey 续期循环
async fn listen_key_renewal_loop<S: WsDataSink>(
    rest_client: Arc<BinanceRestClient>,
    weak_ref: WeakActorRef<BinancePrivateWsActor<S>>,
) {
    let mut interval = tokio::time::interval(LISTEN_KEY_RENEWAL_INTERVAL);
    interval.tick().await; // 跳过首次立即触发

    loop {
        interval.tick().await;

        // 检查 Actor 是否还存活
        if weak_ref.upgrade().is_none() {
            break;
        }

        // 续期 ListenKey
        match rest_client.keep_alive_listen_key().await {
            Ok(()) => {
                tracing::debug!(
                    exchange = %Exchange::Binance,
                    "ListenKey renewed successfully"
                );
            }
            Err(e) => {
                tracing::error!(
                    exchange = %Exchange::Binance,
                    error = %e,
                    "Failed to renew ListenKey, connection may be dropped"
                );
                // 续期失败，触发 Actor 停止重连
                if let Some(actor_ref) = weak_ref.upgrade() {
                    actor_ref.stop_gracefully().await.ok();
                }
                break;
            }
        }
    }
}

// === Messages ===

/// 发送消息到 WebSocket
pub struct SendMessage(pub String);

impl<S: WsDataSink> Message<SendMessage> for BinancePrivateWsActor<S> {
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

/// Binance 私有连接句柄
pub struct BinancePrivateHandle<S: WsDataSink> {
    actor_ref: ActorRef<BinancePrivateWsActor<S>>,
    conn_id: ConnectionId,
}

impl<S: WsDataSink> BinancePrivateHandle<S> {
    pub fn new(actor_ref: ActorRef<BinancePrivateWsActor<S>>, conn_id: ConnectionId) -> Self {
        Self { actor_ref, conn_id }
    }
}

#[async_trait]
impl<S: WsDataSink> PrivateConnectionHandle for BinancePrivateHandle<S> {
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

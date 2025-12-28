//! 私有 WebSocket 连接抽象
//!
//! 定义 PrivateConnectionHandle trait，各交易所实现自己的私有连接 Actor

use super::public_ws::{ConnectionId, WsData, WsDataSink, WsError};
use async_trait::async_trait;
use kameo::actor::ActorID;
use std::sync::Arc;

/// 私有连接句柄 trait (类型擦除)
///
/// ExchangeActor 通过此 trait 与私有连接交互，
/// 无需知道底层是 Binance 还是 OKX 的实现
#[async_trait]
pub trait PrivateConnectionHandle: Send + Sync {
    /// 获取 Actor ID (用于 on_link_died 中识别)
    fn actor_id(&self) -> ActorID;

    /// 获取连接 ID
    fn conn_id(&self) -> ConnectionId;

    /// 发送消息到 WebSocket
    async fn send_message(&self, msg: String) -> bool;
}

/// 私有连接创建结果
pub struct PrivateConnectionResult {
    pub handle: Box<dyn PrivateConnectionHandle>,
    pub actor_id: ActorID,
}

// === 通用的私有连接 ws_loop ===

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 私有连接的 WebSocket 循环
///
/// Binance 和 OKX 的私有连接数据处理逻辑相同，
/// 只是建立连接和认证的方式不同
pub async fn run_private_ws_loop<S: WsDataSink>(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut write_rx: mpsc::Receiver<String>,
    data_sink: Arc<S>,
    conn_id: ConnectionId,
    exchange: crate::domain::Exchange,
    stop_signal: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), WsError> {
    let mut stop_signal = stop_signal;

    loop {
        tokio::select! {
            // 停止信号
            _ = &mut stop_signal => {
                let _ = write.close().await;
                return Ok(());
            }

            // 发送消息
            msg = write_rx.recv() => {
                match msg {
                    Some(text) => {
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            tracing::warn!(
                                exchange = %exchange,
                                conn_id = conn_id.0,
                                "Private WebSocket send failed"
                            );
                            return Err(WsError::Network("Send failed".to_string()));
                        }
                    }
                    None => {
                        let _ = write.close().await;
                        return Ok(());
                    }
                }
            }

            // 接收消息
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
                            "Private WebSocket closed by server"
                        );
                        return Err(WsError::ServerClosed);
                    }
                    Some(Err(e)) => {
                        tracing::warn!(
                            exchange = %exchange,
                            conn_id = conn_id.0,
                            error = %e,
                            "Private WebSocket error"
                        );
                        return Err(WsError::Network(e.to_string()));
                    }
                    None => {
                        tracing::warn!(
                            exchange = %exchange,
                            conn_id = conn_id.0,
                            "Private WebSocket stream ended"
                        );
                        return Err(WsError::ServerClosed);
                    }
                    _ => {}
                }
            }
        }
    }
}

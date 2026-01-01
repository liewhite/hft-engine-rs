//! 公共 WebSocket 循环模块
//!
//! 提供统一的 WebSocket 循环逻辑，消除各 actor 中的代码重复

use crate::exchange::client::WsError;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// WebSocket 循环入站数据
#[derive(Debug)]
pub enum WsIncoming {
    /// 收到文本消息
    Data(String),
    /// 连接出错
    Error(WsError),
}

/// 运行 WebSocket 循环
///
/// # 参数
/// - `read`: WebSocket 读取端
/// - `write`: WebSocket 写入端
/// - `outgoing_rx`: 出站消息接收器 (Subscribe/Unsubscribe 等)
/// - `incoming_tx`: 入站消息发送器 (收到的数据/错误)
///
/// 循环退出时:
/// - 如果出错，发送 `WsIncoming::Error` 到 `incoming_tx`
/// - 如果 `outgoing_rx` 关闭（actor drop 了 sender），正常退出
pub async fn run_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut outgoing_rx: mpsc::Receiver<String>,
    incoming_tx: mpsc::Sender<WsIncoming>,
) {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut outgoing_rx, &incoming_tx).await;

    // 出错时发送错误信号
    if let Err(e) = result {
        let _ = incoming_tx.send(WsIncoming::Error(e)).await;
    }
}

async fn run_ws_loop_inner(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin
              + Send),
    write: &mut (impl SinkExt<WsMessage> + Unpin + Send),
    outgoing_rx: &mut mpsc::Receiver<String>,
    incoming_tx: &mpsc::Sender<WsIncoming>,
) -> Result<(), WsError> {
    loop {
        tokio::select! {
            // 处理出站消息
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(text) => {
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            return Err(WsError::Network("Send failed".to_string()));
                        }
                    }
                    // Actor drop 了 sender，正常退出
                    None => return Ok(()),
                }
            }

            // 处理入站消息
            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        // 发送失败说明 actor 已停止，正常退出
                        if incoming_tx.send(WsIncoming::Data(text)).await.is_err() {
                            return Ok(());
                        }
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
                    // Binary, Frame 等忽略
                    _ => {}
                }
            }
        }
    }
}

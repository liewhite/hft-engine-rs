//! 公共 WebSocket 循环模块
//!
//! 提供统一的 WebSocket 循环逻辑，消除各 actor 中的代码重复

use crate::exchange::client::WsError;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 统一处理 WS 入站 `StreamMessage` 的四个分支，消除各 WS actor 中逐字重复的样板。
///
/// 各 actor 的 `Message<StreamMessage<Result<String, WsError>, (), ()>>` handler 逻辑相同，
/// 仅交易所标签不同：数据到达 → `handle_message` 解析发布，失败/错误/流结束一律记录并
/// `kill` actor（触发 `on_link_died` 级联受控停机，不重连——见错误处理约定）。
///
/// 要求 `$this` 具备 `async fn handle_message(&self, raw: &str) -> Result<(), WsError>`
/// （`&self`/`&mut self` 均可）。IBKR public WS 的 `handle_message` 需额外的 actor_ref
/// 参数，签名不同，故不使用本宏、保留独立实现。
#[macro_export]
macro_rules! dispatch_ws_stream_message {
    ($this:expr, $msg:expr, $ctx:expr, $exchange:literal) => {{
        match $msg {
            kameo::message::StreamMessage::Next(Ok(data)) => {
                if let Err(e) = $this.handle_message(&data).await {
                    tracing::error!(exchange = $exchange, error = %e, raw = %data, "WS parse error, killing actor");
                    $ctx.actor_ref().kill();
                }
            }
            kameo::message::StreamMessage::Next(Err(e)) => {
                tracing::error!(exchange = $exchange, error = %e, "WS loop exited, killing actor");
                $ctx.actor_ref().kill();
            }
            kameo::message::StreamMessage::Started(_) => {
                tracing::debug!(exchange = $exchange, "WS incoming stream started");
            }
            kameo::message::StreamMessage::Finished(_) => {
                tracing::error!(exchange = $exchange, "WS stream unexpectedly finished, killing actor");
                $ctx.actor_ref().kill();
            }
        }
    }};
}

/// 运行 WebSocket 循环
///
/// # 参数
/// - `read`: WebSocket 读取端
/// - `write`: WebSocket 写入端
/// - `outgoing_rx`: 出站消息接收器 (Subscribe/Unsubscribe 等)
/// - `incoming_tx`: 入站消息发送器 (Ok=数据, Err=错误)
///
/// 循环退出时:
/// - 如果出错，发送 `Err(WsError)` 到 `incoming_tx`
/// - 如果 `outgoing_rx` 关闭（actor drop 了 sender），正常退出
pub async fn run_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut outgoing_rx: mpsc::Receiver<String>,
    incoming_tx: mpsc::Sender<Result<String, WsError>>,
) {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut outgoing_rx, &incoming_tx).await;

    // 出错时发送错误信号
    if let Err(e) = result {
        let _ = incoming_tx.send(Err(e)).await;
    }
}

async fn run_ws_loop_inner(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin
              + Send),
    write: &mut (impl SinkExt<WsMessage> + Unpin + Send),
    outgoing_rx: &mut mpsc::Receiver<String>,
    incoming_tx: &mpsc::Sender<Result<String, WsError>>,
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
                        tracing::trace!(text = %text, "ws_loop received message");
                        if incoming_tx.send(Ok(text)).await.is_err() {
                            return Ok(());
                        }
                    }
                    Some(Ok(WsMessage::Binary(data))) => {
                        match String::from_utf8(data.into()) {
                            Ok(text) => {
                                tracing::trace!(text = %text, "ws_loop received binary message");
                                if incoming_tx.send(Ok(text)).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "ws_loop: binary message is not valid UTF-8");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        write.send(WsMessage::Pong(data)).await.map_err(|_| {
                            WsError::Network("Failed to send pong".to_string())
                        })?;
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        let reason = frame
                            .map(|f| format!("code={}, reason={}", f.code, f.reason))
                            .unwrap_or_else(|| "no reason".to_string());
                        return Err(WsError::ServerClosed(reason));
                    }
                    Some(Err(e)) => {
                        return Err(WsError::Network(e.to_string()));
                    }
                    None => {
                        return Err(WsError::ServerClosed("connection dropped".to_string()));
                    }
                    // 其它帧类型 (Pong/Frame 等) 忽略
                    _ => {}
                }
            }
        }
    }
}

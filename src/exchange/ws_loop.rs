//! 公共 WebSocket 循环模块
//!
//! 提供统一的 WebSocket 循环逻辑，消除各 actor 中的代码重复

use crate::exchange::client::WsError;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// WS 保活与断流检测配置。
///
/// 此前整个项目没有应用层心跳与空闲检测：OKX 的空闲连接会被服务端 30 秒静默断开、
/// Hyperliquid 60 秒；而"连接活着但数据停了"（半开连接、上游静默故障）无人能发现 ——
/// 只有 IBKR 自己造了重订阅机制。统一收在 ws_loop：
/// - `ping_text`：周期发送的应用层 ping（各所报文不同，由调用方给出；None = 该所由
///   服务端主动 ping，客户端回 pong 即可，见循环里的 Ping 分支）
/// - `idle_timeout`：超过此时长没有收到**任何**入站帧（含 Ping/Pong 控制帧）即判定
///   断流，按既有 fail-fast 姿态退出（kill -> 级联受控停机，交外部重启），不重连。
///   必须大于该所服务端/我方的 ping 周期，否则安静的私有流会被误杀。
#[derive(Debug, Clone)]
pub struct WsKeepalive {
    /// 周期性发送的应用层 ping 文本；None = 不主动 ping
    pub ping_text: Option<String>,
    /// ping 发送周期（ping_text 为 None 时仅作空闲检查周期）
    pub ping_interval: Duration,
    /// 空闲上限：超过即判定断流退出
    pub idle_timeout: Duration,
}

impl WsKeepalive {
    /// OKX：文本 "ping"，服务端 30 秒无消息断开
    pub fn okx() -> Self {
        Self {
            ping_text: Some("ping".to_string()),
            ping_interval: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(60),
        }
    }

    /// Hyperliquid：`{"method":"ping"}`，服务端 60 秒无消息断开
    pub fn hyperliquid() -> Self {
        Self {
            ping_text: Some(r#"{"method":"ping"}"#.to_string()),
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(90),
        }
    }

    /// Binance：服务端每 ~3 分钟主动 Ping（循环自动回 Pong），客户端无需发 ping；
    /// 空闲上限取服务端 ping 周期的两倍余量
    pub fn binance() -> Self {
        Self {
            ping_text: None,
            ping_interval: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(8 * 60),
        }
    }

    /// IBKR：会话由 tickle 保活（REST），WS 侧只做空闲检测；smd 有 8 分钟重订阅周期，
    /// 空闲上限给足余量
    pub fn ibkr() -> Self {
        Self {
            ping_text: None,
            ping_interval: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(12 * 60),
        }
    }
}

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
/// - `keepalive`: 应用层心跳与空闲检测（见 [`WsKeepalive`]）
///
/// 循环退出时:
/// - 如果出错（含空闲超时），发送 `Err(WsError)` 到 `incoming_tx`
/// - 如果 `outgoing_rx` 关闭（actor drop 了 sender），正常退出
pub async fn run_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut outgoing_rx: mpsc::Receiver<String>,
    incoming_tx: mpsc::Sender<Result<String, WsError>>,
    keepalive: WsKeepalive,
) {
    let result =
        run_ws_loop_inner(&mut read, &mut write, &mut outgoing_rx, &incoming_tx, &keepalive).await;

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
    keepalive: &WsKeepalive,
) -> Result<(), WsError> {
    let mut ping_timer = tokio::time::interval(keepalive.ping_interval);
    ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping_timer.tick().await; // interval 的首个 tick 立即到期，跳过
    let mut last_activity = tokio::time::Instant::now();
    loop {
        tokio::select! {
            // 心跳节拍：发应用层 ping（若该所需要），并检查空闲
            _ = ping_timer.tick() => {
                if last_activity.elapsed() > keepalive.idle_timeout {
                    return Err(WsError::Network(format!(
                        "idle timeout: 已 {:?} 未收到任何入站帧（含控制帧），判定断流",
                        last_activity.elapsed()
                    )));
                }
                if let Some(text) = &keepalive.ping_text {
                    if write.send(WsMessage::Text(text.clone())).await.is_err() {
                        return Err(WsError::Network("Ping send failed".to_string()));
                    }
                }
            }

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

            // 处理入站消息（任何帧都算活动，含 Ping/Pong 控制帧）
            ws_msg = read.next() => {
                last_activity = tokio::time::Instant::now();
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

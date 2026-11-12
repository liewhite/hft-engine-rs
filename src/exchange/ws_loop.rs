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
    /// 空闲上限 8 分钟的含义是**容忍恰好丢一个服务端 ping**（连丢两个即判定断流）——
    /// 调小到 6 分钟以内会把偶发的单个 ping 丢失误杀成停机
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

/// 私有订阅确认的等待上限：超时未集齐即启动失败（受控退出，不重试）
pub const SUBSCRIPTION_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

/// 启动阶段等私有订阅全部确认，供私有 WS actor 在 `on_start` 里调用 —— **请求发出去不等于
/// 订阅生效**，没确认就算启动完成的话，引擎会在私有回报还没接上时开始下单、建持仓基线。
///
/// 在 [`run_ws_loop`] 已经跑起来之后、`attach_stream` 之前调用：心跳、回 Pong、断线判定都由
/// 循环负责，这里只从 `incoming_rx` 读文本帧。`confirms` 由各所给出：一帧确认了哪个订阅
/// （确认回执，或该频道的数据到达 —— 后者是更硬的证据）返回 `Some(订阅名)`，与订阅无关
/// 返回 `None`，服务端拒绝/报错返回 `Err`（启动失败）。
///
/// 返回期间收到的全部文本帧（含回执），调用方须在 `incoming_rx` 之前原样重放给 actor ——
/// 其中可能已有成交回报，丢了会让持仓少记一笔。
pub async fn await_subscriptions(
    incoming_rx: &mut mpsc::Receiver<Result<String, WsError>>,
    required: &[&str],
    confirms: impl Fn(&str) -> Result<Option<String>, WsError>,
) -> Result<Vec<String>, WsError> {
    let mut pending: std::collections::HashSet<&str> = required.iter().copied().collect();
    let mut frames = Vec::new();
    let wait = async {
        while !pending.is_empty() {
            let text = match incoming_rx.recv().await {
                Some(Ok(text)) => text,
                Some(Err(e)) => return Err(e),
                None => return Err(WsError::ServerClosed("等订阅确认时连接已断".to_string())),
            };
            if let Some(sub) = confirms(&text)? {
                if pending.remove(sub.as_str()) {
                    tracing::info!(subscription = %sub, remaining = pending.len(), "私有订阅已确认生效");
                }
            }
            frames.push(text);
        }
        Ok(())
    };
    let outcome = tokio::time::timeout(SUBSCRIPTION_CONFIRM_TIMEOUT, wait).await;
    match outcome {
        Ok(r) => r.map(|()| frames),
        Err(_) => Err(WsError::Network(format!(
            "私有订阅 {pending:?} 未在 {}s 内确认，私有回报不可信",
            SUBSCRIPTION_CONFIRM_TIMEOUT.as_secs()
        ))),
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

#[cfg(test)]
mod keepalive_tests {
    use super::*;
    use futures_util::stream;
    use tokio_stream::wrappers::ReceiverStream;
    use tokio_util::sync::PollSender;

    type Frame = Result<WsMessage, tokio_tungstenite::tungstenite::Error>;

    fn keepalive(ping: Option<&str>, interval_s: u64, idle_s: u64) -> WsKeepalive {
        WsKeepalive {
            ping_text: ping.map(str::to_string),
            ping_interval: Duration::from_secs(interval_s),
            idle_timeout: Duration::from_secs(idle_s),
        }
    }

    /// 空闲超时是**整机 kill 路径**：无任何入站帧时必须在 idle_timeout 后以 Err 退出。
    /// start_paused 下时间自动推进，无需真实等待。
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_exits_with_error() {
        let read = stream::pending::<Frame>();
        let (w_tx, _w_rx) = mpsc::channel::<WsMessage>(64);
        let write = PollSender::new(w_tx);
        let (_out_tx, out_rx) = mpsc::channel::<String>(8); // 保持 sender 存活，避免正常退出
        let (in_tx, mut in_rx) = mpsc::channel::<Result<String, WsError>>(8);

        tokio::spawn(run_ws_loop(read, write, out_rx, in_tx, keepalive(None, 1, 3)));

        let msg = in_rx.recv().await.expect("应收到断流错误");
        let err = msg.expect_err("空闲超时必须是 Err");
        assert!(err.to_string().contains("idle"), "got: {err}");
    }

    /// 任何入站帧（含 Ping 控制帧）都重置空闲计时：帧持续到达时绝不误杀
    #[tokio::test(start_paused = true)]
    async fn incoming_frames_reset_the_idle_clock() {
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(16);
        let read = ReceiverStream::new(frame_rx);
        let (w_tx, _w_rx) = mpsc::channel::<WsMessage>(64);
        let write = PollSender::new(w_tx);
        let (_out_tx, out_rx) = mpsc::channel::<String>(8);
        let (in_tx, mut in_rx) = mpsc::channel::<Result<String, WsError>>(16);

        // 每 2 秒来一帧，idle 上限 3 秒：若计时不被重置，第 3 秒就会误杀
        tokio::spawn(async move {
            for i in 0..5 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = frame_tx.send(Ok(WsMessage::Text(format!("m{i}")))).await;
            }
            // 之后停止喂帧但保持连接（sender 不 drop），等 idle 超时
            tokio::time::sleep(Duration::from_secs(3600)).await;
            drop(frame_tx);
        });
        tokio::spawn(run_ws_loop(read, write, out_rx, in_tx, keepalive(None, 1, 3)));

        // 5 帧全部送达（期间跨了 10 秒 > idle 3 秒，证明计时被入站帧重置）
        for i in 0..5 {
            let msg = in_rx.recv().await.expect("帧应送达");
            assert_eq!(msg.expect("数据帧"), format!("m{i}"));
        }
        // 停止喂帧后，才在 idle 上限处退出
        let err = in_rx.recv().await.expect("应收到断流错误").expect_err("Err");
        assert!(err.to_string().contains("idle"), "got: {err}");
    }

    fn by_prefix(raw: &str) -> Result<Option<String>, WsError> {
        match raw.split_once(':') {
            Some(("ack", sub)) | Some(("data", sub)) => Ok(Some(sub.to_string())),
            Some(("error", e)) => Err(WsError::ParseError(e.to_string())),
            _ => Ok(None),
        }
    }

    /// 回执与数据都算确认；集齐才返回，期间的帧（含数据）全部原样交回供重放，一帧不丢
    #[tokio::test]
    async fn subscriptions_confirmed_by_ack_or_data_and_frames_are_kept() {
        let (tx, mut rx) = mpsc::channel(8);
        for f in ["ack:orders", "noise", "data:fills", "late"] {
            tx.send(Ok(f.to_string())).await.unwrap();
        }
        let frames = await_subscriptions(&mut rx, &["orders", "fills"], by_prefix).await.unwrap();
        assert_eq!(frames, ["ack:orders", "noise", "data:fills"]);
        assert_eq!(rx.recv().await.unwrap().unwrap(), "late", "集齐后的帧留给 actor");
    }

    /// 服务端拒绝、连接断开、超时未集齐都是启动失败
    #[tokio::test(start_paused = true)]
    async fn rejection_disconnect_and_timeout_fail_startup() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(Ok("error:bad sub".to_string())).await.unwrap();
        let e = await_subscriptions(&mut rx, &["orders"], by_prefix).await.unwrap_err();
        assert!(e.to_string().contains("bad sub"), "{e}");

        tx.send(Ok("ack:orders".to_string())).await.unwrap();
        let e = await_subscriptions(&mut rx, &["orders", "fills"], by_prefix).await.unwrap_err();
        assert!(e.to_string().contains("fills"), "超时要点名缺哪条: {e}");

        drop(tx);
        assert!(await_subscriptions(&mut rx, &["orders"], by_prefix).await.is_err());
    }

    /// 配置了 ping_text 的所（OKX/HL）按周期发送应用层 ping
    #[tokio::test(start_paused = true)]
    async fn ping_text_is_sent_periodically() {
        let read = stream::pending::<Frame>();
        let (w_tx, mut w_rx) = mpsc::channel::<WsMessage>(64);
        let write = PollSender::new(w_tx);
        let (_out_tx, out_rx) = mpsc::channel::<String>(8);
        let (in_tx, _in_rx) = mpsc::channel::<Result<String, WsError>>(8);

        tokio::spawn(run_ws_loop(read, write, out_rx, in_tx, keepalive(Some("ping"), 1, 3600)));

        for _ in 0..3 {
            let sent = w_rx.recv().await.expect("应周期性发出 ping");
            assert_eq!(sent, WsMessage::Text("ping".to_string()));
        }
    }
}

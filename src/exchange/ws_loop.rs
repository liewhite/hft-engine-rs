//! 公共 WebSocket 循环模块
//!
//! 提供统一的 WebSocket 循环逻辑（[`WsConn`]），各所 WS 组件共用

use crate::domain::{Exchange, ExchangeError};
use crate::exchange::client::WsError;
use crate::tasks::{Group, GroupClosed, Scope};
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
///   断流，按既有 fail-fast 姿态退出（错误上冒 -> 整机受控停机，交外部重启），不重连。
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

/// WS 入站缓冲：循环读得比组件处理快时在这里攒着，满了循环就暂停读（背压，不丢）
const INCOMING_BUFFER: usize = 100;

/// 一条 WS 连接的组件侧把手：读循环（心跳、回 Pong、空闲判定）作为子任务挂在组件自己的
/// 分组下，组件经它收发文本帧。
///
/// 循环出错（断线、空闲超时、写失败）时子任务以 `Err` 退出、按任务树规则上冒，整支停下
/// （fail-fast，不重连，交外部重启）；组件这边随后从 [`Self::recv`] 拿到"连接已断"的错误。
/// 组件停机时，它的 `run` 先返回、之后框架才停这个子任务，所以收尾期间连接仍在。
///
/// 取帧有两种写法：
/// - [`Self::next`]：感知停机。没有收件箱的组件（各所私有 WS）的整个 `run` 就是
///   `while let Some(raw) = conn.next(scope).await? { .. }`，停机时已到手的回报（可能有成交）
///   会先交完。有收件箱的组件在 `inbox.recv()` 返回 `None` 之后用它排空。
/// - [`Self::recv`]：只等下一帧，供停机信号另有来源（收件箱）的 `select!` 分支用。
pub struct WsConn {
    outgoing: mpsc::Sender<String>,
    incoming: mpsc::Receiver<String>,
    /// [`Self::await_subscriptions`] 期间收到的帧，[`Self::recv`] 先交它们
    replay: std::collections::VecDeque<String>,
}

impl WsConn {
    /// 在 `group` 下起读循环子任务。组件构造时调用（分组只能在构造阶段声明）。
    pub fn spawn(
        group: &Group,
        name: impl Into<String>,
        read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
            + Unpin
            + Send
            + 'static,
        write: impl SinkExt<WsMessage> + Unpin + Send + 'static,
        keepalive: WsKeepalive,
    ) -> Result<Self, GroupClosed> {
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        let (incoming_tx, incoming_rx) = mpsc::channel::<String>(INCOMING_BUFFER);
        group.spawn_fn(name, move |scope| async move {
            tokio::select! {
                _ = scope.stopping() => Ok(()),
                r = run_ws_loop(read, write, outgoing_rx, incoming_tx, keepalive) => {
                    if r.is_ok() {
                        // 组件丢掉了收发端：它已退出（正常停机，或构造/运行出错）。它自己的结果由
                        // 它自己上报，这里按主动收工退出 —— 否则框架把"未经停止请求就自行结束"
                        // 记成错误，可能抢在组件的真正根因（如构造失败）之前报到父节点。
                        scope.request_stop();
                    }
                    r
                }
            }
        })?;
        Ok(Self { outgoing: outgoing_tx, incoming: incoming_rx, replay: Default::default() })
    }

    /// 发一帧文本（订阅/退订请求等）。循环已退出时返回 `Err`。
    pub async fn send(&self, text: String) -> Result<(), WsError> {
        self.outgoing
            .send(text)
            .await
            .map_err(|_| WsError::Network("WS 循环已退出，发送失败".to_string()))
    }

    /// 下一帧入站文本。循环已退出（断线等，原因由循环子任务自己上报）时返回 `Err`。
    /// 可安全地放在 `select!` 里（取消不丢帧）。
    pub async fn recv(&mut self) -> Result<String, WsError> {
        if let Some(text) = self.replay.pop_front() {
            return Ok(text);
        }
        self.incoming
            .recv()
            .await
            .ok_or_else(|| WsError::ServerClosed("WS 循环已退出（原因见循环子任务的错误）".to_string()))
    }

    /// 感知停机的取帧：`scope`（组件自己的）未被请求停止时等下一帧（同 [`Self::recv`]）；
    /// 已被请求停止时只交已经缓冲在手、还没处理的帧，交完返回 `Ok(None)`。
    ///
    /// 停机时把已到手的帧交完是为了不丢回报：私有流里可能有成交，而读循环在组件 `run`
    /// 返回后才停，缓冲里的帧没人取就随连接一起没了。停机期间循环仍在读，紧接着到的帧
    /// 也会被交出；循环已退出时交完缓冲即 `None`（断线原因由循环子任务自己上报）。
    pub async fn next(&mut self, scope: &Scope) -> Result<Option<String>, WsError> {
        if !scope.is_stopping() {
            tokio::select! {
                // 帧优先：与停止信号同时就绪时不跳过（反正之后也要排空）
                biased;
                frame = self.recv() => return frame.map(Some),
                _ = scope.stopping() => {}
            }
        }
        Ok(self.replay.pop_front().or_else(|| self.incoming.try_recv().ok()))
    }

    /// 启动阶段等私有订阅全部确认，供私有 WS 组件在构造时调用 —— **请求发出去不等于
    /// 订阅生效**，没确认就算启动完成的话，引擎会在私有回报还没接上时开始下单、建持仓基线。
    ///
    /// 心跳、回 Pong、断线判定都由读循环负责，这里只读文本帧。`confirms` 由各所给出：一帧
    /// 确认了哪个订阅（确认回执，或该频道的数据到达 —— 后者是更硬的证据）返回
    /// `Some(订阅名)`，与订阅无关返回 `None`，服务端拒绝/报错返回 `Err`（启动失败）。
    ///
    /// 确认期间收到的全部帧留着，之后 [`Self::recv`] 先交它们：确认期间可能已有成交回报，
    /// 这样不会漏掉。
    pub async fn await_subscriptions(
        &mut self,
        exchange: Exchange,
        required: &[&str],
        confirms: impl Fn(&str) -> Result<Option<String>, WsError>,
    ) -> Result<(), ExchangeError> {
        let mut pending: std::collections::HashSet<&str> = required.iter().copied().collect();
        let incoming = &mut self.incoming;
        let replay = &mut self.replay;
        let wait = async {
            while !pending.is_empty() {
                let text = incoming
                    .recv()
                    .await
                    .ok_or_else(|| WsError::ServerClosed("等订阅确认时连接已断".to_string()))?;
                if let Some(sub) = confirms(&text)? {
                    if pending.remove(sub.as_str()) {
                        tracing::info!(%exchange, subscription = %sub, remaining = pending.len(), "私有订阅已确认生效");
                    }
                }
                replay.push_back(text);
            }
            Ok::<(), WsError>(())
        };
        match tokio::time::timeout(SUBSCRIPTION_CONFIRM_TIMEOUT, wait).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(ExchangeError::WebSocketError(format!("{exchange} 私有订阅: {e}"))),
            Err(_) => Err(ExchangeError::Timeout(format!(
                "{exchange} 私有订阅 {pending:?} 未在 {}s 内确认，私有回报不可信",
                SUBSCRIPTION_CONFIRM_TIMEOUT.as_secs()
            ))),
        }
    }
}

/// 私有订阅确认的等待上限：超时未集齐即启动失败（受控退出，不重试）
pub const SUBSCRIPTION_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

/// 读循环：转发出站帧、把入站文本帧交给组件、心跳与空闲判定。
///
/// 返回 `Err` = 连接不可用（断线、服务端关闭、空闲超时、写失败）。组件丢掉了收发两端
/// （只会发生在组件已退出之后）时返回 `Ok`（调用方据此主动收工，见 [`WsConn::spawn`]）。
async fn run_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut outgoing_rx: mpsc::Receiver<String>,
    incoming_tx: mpsc::Sender<String>,
    keepalive: WsKeepalive,
) -> anyhow::Result<()> {
    run_ws_loop_inner(&mut read, &mut write, &mut outgoing_rx, &incoming_tx, &keepalive)
        .await
        .map_err(|e| anyhow::anyhow!("WS 连接: {e}"))
}

async fn run_ws_loop_inner(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin
              + Send),
    write: &mut (impl SinkExt<WsMessage> + Unpin + Send),
    outgoing_rx: &mut mpsc::Receiver<String>,
    incoming_tx: &mpsc::Sender<String>,
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
                    // 组件丢了发送端（组件已退出），无人再收
                    None => return Ok(()),
                }
            }

            // 处理入站消息（任何帧都算活动，含 Ping/Pong 控制帧）
            ws_msg = read.next() => {
                last_activity = tokio::time::Instant::now();
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        tracing::trace!(text = %text, "ws_loop received message");
                        if incoming_tx.send(text).await.is_err() {
                            return Ok(());
                        }
                    }
                    Some(Ok(WsMessage::Binary(data))) => {
                        match String::from_utf8(data.into()) {
                            Ok(text) => {
                                tracing::trace!(text = %text, "ws_loop received binary message");
                                if incoming_tx.send(text).await.is_err() {
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

    /// 空闲超时是**整机停机路径**：无任何入站帧时必须在 idle_timeout 后以 Err 退出。
    /// start_paused 下时间自动推进，无需真实等待。
    #[tokio::test(start_paused = true)]
    async fn idle_timeout_exits_with_error() {
        let read = stream::pending::<Frame>();
        let (w_tx, _w_rx) = mpsc::channel::<WsMessage>(64);
        let write = PollSender::new(w_tx);
        let (_out_tx, out_rx) = mpsc::channel::<String>(8); // 保持 sender 存活，避免正常退出
        let (in_tx, _in_rx) = mpsc::channel::<String>(8);

        let err = run_ws_loop(read, write, out_rx, in_tx, keepalive(None, 1, 3))
            .await
            .expect_err("空闲超时必须是 Err");
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
        let (in_tx, mut in_rx) = mpsc::channel::<String>(16);

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
        let ws = tokio::spawn(run_ws_loop(read, write, out_rx, in_tx, keepalive(None, 1, 3)));

        // 5 帧全部送达（期间跨了 10 秒 > idle 3 秒，证明计时被入站帧重置）
        for i in 0..5 {
            assert_eq!(in_rx.recv().await.expect("帧应送达"), format!("m{i}"));
        }
        // 停止喂帧后，才在 idle 上限处退出
        let err = ws.await.unwrap().expect_err("Err");
        assert!(err.to_string().contains("idle"), "got: {err}");
    }

    /// 循环出错（断线）经任务树上冒：根停机、错误带着原因；组件侧 recv 随之报连接已断
    #[tokio::test]
    async fn loop_failure_propagates_through_the_tree() {
        let root = crate::tasks::Root::new("t");
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>(4);
        let (w_tx, _w_rx) = mpsc::channel::<WsMessage>(64);
        let mut conn = WsConn::spawn(
            &root.group("g"),
            "ws",
            ReceiverStream::new(frame_rx),
            PollSender::new(w_tx),
            keepalive(None, 60, 3600),
        )
        .unwrap();
        frame_tx.send(Ok(WsMessage::Text("a".into()))).await.unwrap();
        assert_eq!(conn.recv().await.unwrap(), "a");
        drop(frame_tx); // 连接断开
        assert!(conn.recv().await.is_err());
        root.stopping().await;
        let e = root.shutdown().await.unwrap_err();
        assert!(format!("{e:#}").contains("connection dropped"), "{e:#}");
    }

    fn by_prefix(raw: &str) -> Result<Option<String>, WsError> {
        match raw.split_once(':') {
            Some(("ack", sub)) | Some(("data", sub)) => Ok(Some(sub.to_string())),
            Some(("error", e)) => Err(WsError::ParseError(e.to_string())),
            _ => Ok(None),
        }
    }

    /// 不经读循环、直接喂入站帧的连接（只测订阅确认）
    fn conn_fed_by(frames: &[&str]) -> (WsConn, mpsc::Sender<String>) {
        let (in_tx, in_rx) = mpsc::channel(8);
        for f in frames {
            in_tx.try_send(f.to_string()).unwrap();
        }
        let (out_tx, _) = mpsc::channel(1);
        (WsConn { outgoing: out_tx, incoming: in_rx, replay: Default::default() }, in_tx)
    }

    /// 回执与数据都算确认；集齐才返回，之后 recv 先给确认期间的全部帧（含数据），再接后续帧，一帧不丢
    #[tokio::test]
    async fn subscriptions_confirmed_by_ack_or_data_and_frames_are_kept() {
        let (mut conn, _tx) = conn_fed_by(&["ack:orders", "noise", "data:fills", "late"]);
        conn.await_subscriptions(Exchange::OKX, &["orders", "fills"], by_prefix).await.unwrap();
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(conn.recv().await.unwrap());
        }
        assert_eq!(got, ["ack:orders", "noise", "data:fills", "late"]);
    }

    /// 服务端拒绝、连接断开、超时未集齐都是启动失败
    #[tokio::test(start_paused = true)]
    async fn rejection_disconnect_and_timeout_fail_startup() {
        let (mut conn, _tx) = conn_fed_by(&["error:bad sub"]);
        let e = conn.await_subscriptions(Exchange::OKX, &["orders"], by_prefix).await.err().unwrap();
        assert!(e.to_string().contains("bad sub"), "{e}");

        let (mut conn, _tx) = conn_fed_by(&["ack:orders"]);
        let e = conn.await_subscriptions(Exchange::OKX, &["orders", "fills"], by_prefix).await.err().unwrap();
        assert!(matches!(e, ExchangeError::Timeout(_)) && e.to_string().contains("fills"), "超时要点名缺哪条: {e}");

        let (mut conn, tx) = conn_fed_by(&[]);
        drop(tx);
        assert!(conn.await_subscriptions(Exchange::OKX, &["orders"], by_prefix).await.is_err());
    }

    /// 配置了 ping_text 的所（OKX/HL）按周期发送应用层 ping
    #[tokio::test(start_paused = true)]
    async fn ping_text_is_sent_periodically() {
        let read = stream::pending::<Frame>();
        let (w_tx, mut w_rx) = mpsc::channel::<WsMessage>(64);
        let write = PollSender::new(w_tx);
        let (_out_tx, out_rx) = mpsc::channel::<String>(8);
        let (in_tx, _in_rx) = mpsc::channel::<String>(8);

        tokio::spawn(run_ws_loop(read, write, out_rx, in_tx, keepalive(Some("ping"), 1, 3600)));

        for _ in 0..3 {
            let sent = w_rx.recv().await.expect("应周期性发出 ping");
            assert_eq!(sent, WsMessage::Text("ping".to_string()));
        }
    }

    /// 停机时 next 交出已缓冲的帧、交完返回 None —— 即使连接还开着（不会一直等新帧）
    #[tokio::test]
    async fn next_hands_over_buffered_frames_after_stop() {
        let root = crate::tasks::Root::new("t");
        let (feed_tx, feed_rx) = tokio::sync::oneshot::channel();
        let (got_tx, got_rx) = tokio::sync::oneshot::channel();
        root.group("g")
            .spawn_fn("c", move |scope| async move {
                let (mut conn, tx) = conn_fed_by(&["a"]);
                // 运行中：同 recv
                assert_eq!(conn.next(&scope).await?.as_deref(), Some("a"));
                feed_tx.send(tx).unwrap();
                scope.stopping().await;
                let mut got = Vec::new();
                while let Some(raw) = conn.next(&scope).await? {
                    got.push(raw);
                }
                got_tx.send(got).unwrap();
                Ok(())
            })
            .unwrap();
        let tx = feed_rx.await.unwrap();
        tx.send("b".into()).await.unwrap();
        tx.send("c".into()).await.unwrap();
        // tx 一直活着：next 若在停机后还等新帧，这里会挂住
        root.shutdown().await.unwrap();
        assert_eq!(got_rx.await.unwrap(), ["b", "c"]);
        drop(tx);
    }

    /// 组件构造失败（已起读循环、随即丢掉连接）：根上报的是构造错误，而不是读循环的
    /// "未经停止请求就自行结束"
    #[tokio::test]
    async fn build_failure_stays_the_root_cause() {
        struct Never;
        impl crate::tasks::Task for Never {
            async fn run(&mut self, _: &Scope) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let root = crate::tasks::Root::new("t");
        let (w_tx, _w_rx) = mpsc::channel::<WsMessage>(8);
        let r = root
            .group("g")
            .spawn("private_ws", move |scope| async move {
                let conn = WsConn::spawn(
                    &scope.group("conn"),
                    "ws",
                    stream::pending::<Frame>(),
                    PollSender::new(w_tx),
                    keepalive(None, 60, 3600),
                )?;
                drop(conn);
                // 让读循环先看到收发端被丢、先退出，再报构造失败（最容易抢掉根因的时序）
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err::<Never, _>(anyhow::anyhow!("订阅被拒"))
            })
            .await;
        assert!(r.is_err());
        let e = format!("{:#}", root.shutdown().await.unwrap_err());
        assert!(e.contains("订阅被拒"), "{e}");
        assert!(!e.contains("未经停止请求"), "{e}");
    }
}

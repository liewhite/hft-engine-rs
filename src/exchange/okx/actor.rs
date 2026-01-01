//! OKX WebSocket Actor

use crate::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsEvent, WsPush,
};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use base64::{engine::general_purpose, Engine as _};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";

/// Private WebSocket URL
const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";

// ============================================================================
// 协程退出信号
// ============================================================================

/// 协程退出信号
#[derive(Debug)]
enum TaskExit {
    PublicWs(WsError),
    PrivateWs(WsError),
}

// ============================================================================
// WebSocket 连接
// ============================================================================

struct WsConnection {
    tx: mpsc::Sender<String>,
}

// ============================================================================
// OkxActor
// ============================================================================

/// OKX Actor 参数
pub struct OkxActorArgs {
    pub credentials: Option<OkxCredentials>,
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    pub event_sink: Arc<dyn EventSink>,
}

/// OKX 凭证
#[derive(Clone)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl OkxCredentials {
    /// WebSocket 登录签名
    fn sign_ws_login(&self, timestamp: &str) -> String {
        let message = format!("{}GET/users/self/verify", timestamp);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any size");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        general_purpose::STANDARD.encode(result.into_bytes())
    }
}

/// OKX Actor
pub struct OkxActor {
    credentials: Option<OkxCredentials>,
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    event_sink: Arc<dyn EventSink>,

    // 连接管理
    public_conn: Option<WsConnection>,
    private_conn: Option<WsConnection>,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,

    /// 协程退出信号发送器
    task_exit_tx: Option<mpsc::Sender<TaskExit>>,
}

impl OkxActor {
    pub fn new(args: OkxActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: args.event_sink,
            public_conn: None,
            private_conn: None,
            subscribed: HashSet::new(),
            task_exit_tx: None,
        }
    }

    async fn create_public_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
        task_exit_tx: mpsc::Sender<TaskExit>,
    ) -> Result<(), WsError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();
        let (tx, rx) = mpsc::channel::<String>(100);

        let weak_ref = actor_ref.downgrade();
        tokio::spawn(run_ws_loop(read, write, rx, weak_ref, task_exit_tx, false));

        self.public_conn = Some(WsConnection { tx });
        Ok(())
    }

    async fn create_private_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
        task_exit_tx: mpsc::Sender<TaskExit>,
    ) -> Result<(), WsError> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or_else(|| WsError::AuthFailed("No credentials".to_string()))?
            .clone();

        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PRIVATE_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 发送 login 消息
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let sign = credentials.sign_ws_login(&timestamp);

        let login_msg = json!({
            "op": "login",
            "args": [{
                "apiKey": credentials.api_key,
                "passphrase": credentials.passphrase,
                "timestamp": timestamp,
                "sign": sign
            }]
        })
        .to_string();

        write
            .send(WsMessage::Text(login_msg))
            .await
            .map_err(|e| WsError::AuthFailed(e.to_string()))?;

        // 等待 login 响应
        loop {
            match read.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                        if event.event == "login" {
                            if event.code.as_deref() == Some("0") {
                                tracing::info!("OKX private login success");
                                break;
                            } else {
                                return Err(WsError::AuthFailed(format!(
                                    "Login failed: {:?}",
                                    event.msg
                                )));
                            }
                        }
                    }
                }
                Some(Ok(WsMessage::Ping(data))) => {
                    write
                        .send(WsMessage::Pong(data))
                        .await
                        .map_err(|e| WsError::Network(format!("Failed to send pong: {}", e)))?;
                }
                Some(Err(e)) => {
                    return Err(WsError::Network(e.to_string()));
                }
                None => {
                    return Err(WsError::ServerClosed);
                }
                _ => {}
            }
        }

        // 订阅私有频道
        let subscribe_msg = json!({
            "op": "subscribe",
            "args": [
                {"channel": "positions", "instType": "SWAP"},
                {"channel": "account"},
                {"channel": "orders", "instType": "SWAP"}
            ]
        })
        .to_string();

        write
            .send(WsMessage::Text(subscribe_msg))
            .await
            .map_err(|e| WsError::Network(e.to_string()))?;

        let (tx, rx) = mpsc::channel::<String>(100);

        let weak_ref = actor_ref.downgrade();
        tokio::spawn(run_ws_loop(read, write, rx, weak_ref, task_exit_tx, true));

        self.private_conn = Some(WsConnection { tx });

        Ok(())
    }

    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind);
        let msg = json!({
            "op": "subscribe",
            "args": [arg]
        })
        .to_string();

        let conn = self
            .public_conn
            .as_ref()
            .expect("public_conn must exist after on_start");
        conn.tx
            .send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind);
        let msg = json!({
            "op": "unsubscribe",
            "args": [arg]
        })
        .to_string();

        let conn = self
            .public_conn
            .as_ref()
            .expect("public_conn must exist after on_start");
        conn.tx
            .send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }
}

impl Actor for OkxActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 创建协程退出信号 channel
        let (task_exit_tx, task_exit_rx) = mpsc::channel::<TaskExit>(8);
        self.task_exit_tx = Some(task_exit_tx.clone());

        // 使用 attach_stream 管理协程生命周期
        let task_exit_stream = ReceiverStream::new(task_exit_rx);
        actor_ref.attach_stream(task_exit_stream, (), ());

        // 1. 创建 public 连接
        self.create_public_connection(&actor_ref, task_exit_tx.clone())
            .await?;

        // 2. 如果有凭证，创建 private 连接
        if self.credentials.is_some() {
            self.create_private_connection(&actor_ref, task_exit_tx)
                .await?;
        }

        tracing::info!(
            exchange = "OKX",
            has_private = self.private_conn.is_some(),
            "OkxActor started"
        );

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // Drop task_exit_tx 会导致 stream 结束
        self.task_exit_tx.take();
        tracing::info!("OkxActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for OkxActor {
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

impl Message<Unsubscribe> for OkxActor {
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

pub struct WsData {
    pub data: String,
}

impl Message<WsData> for OkxActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        let timestamp = now_ms();
        match parse_message(&msg.data, timestamp, &self.symbol_metas) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %msg.data, "Failed to parse OKX message");
            }
        }
    }
}

/// 协程退出信号处理
impl Message<StreamMessage<TaskExit, (), ()>> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<TaskExit, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(exit) => {
                // 任何协程退出都 kill actor
                match exit {
                    TaskExit::PublicWs(e) => {
                        tracing::error!(error = %e, "Public WebSocket loop exited, killing actor");
                    }
                    TaskExit::PrivateWs(e) => {
                        tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
                    }
                }
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Task exit stream started");
            }
            StreamMessage::Finished(_) => {
                // 所有 sender 都 drop 了，说明 actor 正在停止
                tracing::debug!("Task exit stream finished");
            }
        }
    }
}

// ============================================================================
// WebSocket 循环
// ============================================================================

async fn run_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut rx: mpsc::Receiver<String>,
    actor_ref: WeakActorRef<OkxActor>,
    task_exit_tx: mpsc::Sender<TaskExit>,
    is_private: bool,
) {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut rx, &actor_ref).await;

    // 出错时发送退出信号
    if let Err(e) = result {
        let exit = if is_private {
            TaskExit::PrivateWs(e)
        } else {
            TaskExit::PublicWs(e)
        };
        // 发送失败说明 actor 已停止，无需处理
        let _ = task_exit_tx.send(exit).await;
    }
}

async fn run_ws_loop_inner(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin
              + Send),
    write: &mut (impl SinkExt<WsMessage> + Unpin + Send),
    rx: &mut mpsc::Receiver<String>,
    actor_ref: &WeakActorRef<OkxActor>,
) -> Result<(), WsError> {
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(text) => {
                        if write.send(WsMessage::Text(text)).await.is_err() {
                            return Err(WsError::Network("Send failed".to_string()));
                        }
                    }
                    None => return Ok(()),
                }
            }

            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        let Some(actor) = actor_ref.upgrade() else {
                            // Actor 已死，正常退出
                            return Ok(());
                        };
                        actor.tell(WsData { data: text }).await.map_err(|e| {
                            WsError::Network(format!("Failed to tell actor: {}", e))
                        })?;
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
                    _ => {}
                }
            }
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

fn kind_to_arg(kind: &SubscriptionKind) -> serde_json::Value {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            json!({
                "channel": "funding-rate",
                "instId": symbol.to_okx()
            })
        }
        SubscriptionKind::BBO { symbol } => {
            json!({
                "channel": "bbo-tbt",
                "instId": symbol.to_okx()
            })
        }
    }
}

fn parse_message(
    raw: &str,
    local_ts: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是事件响应（控制消息，返回空 Vec）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => return Ok(Vec::new()),
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX error: code={}, msg={}",
                    code, msg
                )));
            }
            _ => return Ok(Vec::new()),
        }
    }

    // 获取频道
    let channel = value
        .get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing channel: {}", raw)))?;

    match channel {
        "funding-rate" => {
            let push: WsPush<FundingRateData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("funding-rate parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let rate = data.to_funding_rate();
                    // funding-rate 没有事件时间戳，使用本地时间
                    IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::FundingRate(rate),
                    }
                })
                .collect();
            Ok(events)
        }
        "bbo-tbt" => {
            let push: WsPush<BboData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bbo-tbt parse: {}", e)))?;
            let inst_id = push
                .arg
                .inst_id
                .as_ref()
                .ok_or_else(|| WsError::ParseError("Missing instId in bbo-tbt".into()))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    // 提取交易所时间戳 (data.ts 是字符串)
                    let exchange_ts = data
                        .ts
                        .parse::<u64>()
                        .unwrap_or_else(|_| panic!("Failed to parse BBO timestamp: {}", data.ts));
                    let bbo = data.to_bbo(inst_id);
                    IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::BBO(bbo),
                    }
                })
                .collect();
            Ok(events)
        }
        "positions" => {
            let push: WsPush<PositionData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("positions parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let mut position = data.to_position();
                    // qty 归一化: 张 -> 币
                    let meta = symbol_metas
                        .get(&position.symbol)
                        .expect("SymbolMeta not found for position symbol");
                    position.size = meta.qty_to_coin(position.size);
                    // positions 没有暴露时间戳字段，使用本地时间
                    IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(position),
                    }
                })
                .collect();
            Ok(events)
        }
        "account" => {
            let push: WsPush<AccountData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("account parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    // 提取交易所时间戳 (u_time 是字符串)
                    let exchange_ts = data.u_time.parse::<u64>().unwrap_or_else(|_| {
                        panic!("Failed to parse account timestamp: {}", data.u_time)
                    });
                    let equity = data.to_equity();
                    IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::Equity {
                            exchange: Exchange::OKX,
                            equity,
                        },
                    }
                })
                .collect();
            Ok(events)
        }
        "orders" => {
            let push: WsPush<OrderPushData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("orders parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let update = data.to_order_update();
                    // orders 没有暴露时间戳字段，使用本地时间
                    IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::OrderUpdate(update),
                    }
                })
                .collect();
            Ok(events)
        }
        _ => {
            // 未知频道，记录警告但不报错
            tracing::warn!(channel, raw, "Unknown OKX channel");
            Ok(Vec::new())
        }
    }
}

//! OKX WebSocket Actor

use crate::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::messaging::ExchangeEvent;
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsEvent, WsPush,
};
use base64::{engine::general_purpose, Engine as _};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use serde_json::json;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 重连延迟 (3 秒)
const RECONNECT_DELAY_SECS: u64 = 3;

/// 单个 WebSocket 连接的最大订阅数
const MAX_SUBSCRIPTIONS_PER_CONN: usize = 100;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";

/// Private WebSocket URL
const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";

// ============================================================================
// WebSocket 连接
// ============================================================================

struct WsConnection {
    tx: mpsc::Sender<String>,
    subscription_count: usize,
    _handle: JoinHandle<()>,
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

/// OKX Actor (无泛型，使用 Arc<dyn EventSink>)
pub struct OkxActor {
    credentials: Option<OkxCredentials>,
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    event_sink: Arc<dyn EventSink>,

    // 连接管理
    public_conns: Vec<WsConnection>,
    private_conn: Option<WsConnection>,
    subscriptions: HashMap<SubscriptionKind, usize>,

    self_ref: Option<WeakActorRef<Self>>,
}

impl OkxActor {
    pub fn new(args: OkxActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: args.event_sink,
            public_conns: Vec::new(),
            private_conn: None,
            subscriptions: HashMap::new(),
            self_ref: None,
        }
    }

    async fn create_public_connection(&mut self) -> Result<usize, WsError> {
        let self_ref = self
            .self_ref
            .as_ref()
            .ok_or_else(|| WsError::ConnectionFailed("Actor not started".to_string()))?
            .clone();

        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();
        let (tx, rx) = mpsc::channel::<String>(100);

        let conn_idx = self.public_conns.len();

        let handle = tokio::spawn(async move {
            if let Err(e) = run_public_ws_loop(read, write, rx, self_ref, conn_idx).await {
                tracing::warn!(conn_idx, error = %e, "OKX public ws_loop ended with error");
            }
        });

        let conn = WsConnection {
            tx,
            subscription_count: 0,
            _handle: handle,
        };

        self.public_conns.push(conn);
        Ok(self.public_conns.len() - 1)
    }

    async fn create_private_connection(&mut self) -> Result<(), WsError> {
        // 中止旧的 private 连接任务 (防止重连时任务泄漏)
        if let Some(conn) = self.private_conn.take() {
            conn._handle.abort();
        }

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
                    let _ = write.send(WsMessage::Pong(data)).await;
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

        let self_ref = self
            .self_ref
            .as_ref()
            .ok_or_else(|| WsError::ConnectionFailed("Actor not started".to_string()))?
            .clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = run_private_ws_loop(read, write, rx, self_ref).await {
                tracing::warn!(error = %e, "OKX private ws_loop ended with error");
            }
        });

        self.private_conn = Some(WsConnection {
            tx,
            subscription_count: 0,
            _handle: handle,
        });

        Ok(())
    }

    async fn ensure_public_connection(&mut self) -> Result<usize, WsError> {
        for (idx, conn) in self.public_conns.iter().enumerate() {
            if conn.subscription_count < MAX_SUBSCRIPTIONS_PER_CONN {
                return Ok(idx);
            }
        }
        self.create_public_connection().await
    }

    async fn send_subscribe(&mut self, conn_idx: usize, kind: &SubscriptionKind) {
        let msg = build_subscribe_msg(&[kind.clone()]);
        if msg.is_empty() {
            return;
        }

        if let Some(conn) = self.public_conns.get_mut(conn_idx) {
            if conn.tx.send(msg).await.is_ok() {
                conn.subscription_count += 1;
            }
        }
    }

    async fn send_unsubscribe(&mut self, conn_idx: usize, kind: &SubscriptionKind) {
        let msg = build_unsubscribe_msg(&[kind.clone()]);
        if msg.is_empty() {
            return;
        }

        if let Some(conn) = self.public_conns.get_mut(conn_idx) {
            if conn.tx.send(msg).await.is_ok() {
                conn.subscription_count = conn.subscription_count.saturating_sub(1);
            }
        }
    }

    /// 重新连接指定的 public 连接 (替换原有连接)
    async fn reconnect_public_connection(&mut self, conn_idx: usize) -> Result<(), WsError> {
        let self_ref = self
            .self_ref
            .as_ref()
            .ok_or_else(|| WsError::ConnectionFailed("Actor not started".to_string()))?
            .clone();

        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();
        let (tx, rx) = mpsc::channel::<String>(100);

        let handle = tokio::spawn(async move {
            if let Err(e) = run_public_ws_loop(read, write, rx, self_ref, conn_idx).await {
                tracing::warn!(conn_idx, error = %e, "OKX public ws_loop ended with error");
            }
        });

        // 获取该连接之前的订阅数量
        let subscription_count = self
            .subscriptions
            .iter()
            .filter(|(_, &idx)| idx == conn_idx)
            .count();

        // 替换连接
        if conn_idx < self.public_conns.len() {
            // 中止旧连接
            self.public_conns[conn_idx]._handle.abort();
            self.public_conns[conn_idx] = WsConnection {
                tx,
                subscription_count,
                _handle: handle,
            };
        }

        Ok(())
    }
}

impl Actor for OkxActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.downgrade());

        // 1. 创建第一个 public 连接
        self.create_public_connection().await?;

        // 2. 如果有凭证，创建 private 连接
        if self.credentials.is_some() {
            self.create_private_connection().await?;
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
        // 中止所有 public 连接的任务
        for conn in self.public_conns.drain(..) {
            conn._handle.abort();
        }

        // 中止 private 连接的任务
        if let Some(conn) = self.private_conn.take() {
            conn._handle.abort();
        }

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
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if self.subscriptions.contains_key(&msg.kind) {
            return;
        }

        let conn_idx = match self.ensure_public_connection().await {
            Ok(idx) => idx,
            Err(e) => {
                tracing::error!(error = %e, "Failed to ensure public connection");
                return;
            }
        };

        self.send_subscribe(conn_idx, &msg.kind).await;
        self.subscriptions.insert(msg.kind, conn_idx);
    }
}

impl Message<Unsubscribe> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let conn_idx = match self.subscriptions.remove(&msg.kind) {
            Some(idx) => idx,
            None => return,
        };
        self.send_unsubscribe(conn_idx, &msg.kind).await;
    }
}

pub struct WsData {
    pub data: String,
}

/// 触发 public 连接重连 (延迟后执行)
struct ReconnectPublic {
    conn_idx: usize,
}

/// 触发 private 连接重连 (延迟后执行)
struct ReconnectPrivate;

/// 执行 public 连接重连
struct DoReconnectPublic {
    conn_idx: usize,
}

/// 执行 private 连接重连
struct DoReconnectPrivate;

impl Message<ReconnectPublic> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: ReconnectPublic,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!(conn_idx = msg.conn_idx, "OKX public connection lost, will reconnect in {}s", RECONNECT_DELAY_SECS);

        // 非阻塞：spawn 延迟任务，延迟后发送 DoReconnectPublic
        let self_ref = self.self_ref.clone();
        let conn_idx = msg.conn_idx;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
            if let Some(actor) = self_ref.and_then(|r| r.upgrade()) {
                let _ = actor.tell(DoReconnectPublic { conn_idx }).await;
            }
        });
    }
}

impl Message<DoReconnectPublic> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: DoReconnectPublic,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!(conn_idx = msg.conn_idx, "Reconnecting OKX public connection...");

        // 收集该连接上的订阅
        let subs_to_restore: Vec<SubscriptionKind> = self
            .subscriptions
            .iter()
            .filter(|(_, &idx)| idx == msg.conn_idx)
            .map(|(kind, _)| kind.clone())
            .collect();

        // 重新创建连接
        match self.reconnect_public_connection(msg.conn_idx).await {
            Ok(()) => {
                // 重新订阅
                for kind in subs_to_restore {
                    self.send_subscribe(msg.conn_idx, &kind).await;
                }
                tracing::info!(conn_idx = msg.conn_idx, "OKX public connection reconnected");
            }
            Err(e) => {
                tracing::error!(conn_idx = msg.conn_idx, error = %e, "Failed to reconnect OKX public connection, retrying...");
                // 再次尝试重连 (通过 ReconnectPublic 触发延迟)
                if let Some(actor) = self.self_ref.as_ref().and_then(|r| r.upgrade()) {
                    let _ = actor.tell(ReconnectPublic { conn_idx: msg.conn_idx }).await;
                }
            }
        }
    }
}

impl Message<ReconnectPrivate> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: ReconnectPrivate,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("OKX private connection lost, will reconnect in {}s", RECONNECT_DELAY_SECS);

        // 非阻塞：spawn 延迟任务，延迟后发送 DoReconnectPrivate
        let self_ref = self.self_ref.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_DELAY_SECS)).await;
            if let Some(actor) = self_ref.and_then(|r| r.upgrade()) {
                let _ = actor.tell(DoReconnectPrivate).await;
            }
        });
    }
}

impl Message<DoReconnectPrivate> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: DoReconnectPrivate,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        tracing::info!("Reconnecting OKX private connection...");

        // 重新创建 private 连接 (包含 login 和私有频道订阅)
        match self.create_private_connection().await {
            Ok(()) => {
                tracing::info!("OKX private connection reconnected");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to reconnect OKX private connection, retrying...");
                // 再次尝试重连 (通过 ReconnectPrivate 触发延迟)
                if let Some(actor) = self.self_ref.as_ref().and_then(|r| r.upgrade()) {
                    let _ = actor.tell(ReconnectPrivate).await;
                }
            }
        }
    }
}

impl Message<WsData> for OkxActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        let timestamp = now_ms();
        if let Some(event) = parse_message(&msg.data, timestamp, &self.symbol_metas) {
            self.event_sink.send_event(event).await;
        }
    }
}

// ============================================================================
// WebSocket 循环
// ============================================================================

async fn run_public_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut rx: mpsc::Receiver<String>,
    actor_ref: WeakActorRef<OkxActor>,
    conn_idx: usize,
) -> Result<(), WsError> {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut rx, &actor_ref).await;

    // 出错时通知 Actor 重连
    if result.is_err() {
        if let Some(actor) = actor_ref.upgrade() {
            let _ = actor.tell(ReconnectPublic { conn_idx }).await;
        }
    }

    result
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
                        if let Some(actor) = actor_ref.upgrade() {
                            let _ = actor.tell(WsData { data: text }).await;
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
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

async fn run_private_ws_loop(
    mut read: impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send,
    mut write: impl SinkExt<WsMessage> + Unpin + Send,
    mut rx: mpsc::Receiver<String>,
    actor_ref: WeakActorRef<OkxActor>,
) -> Result<(), WsError> {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut rx, &actor_ref).await;

    // 出错时通知 Actor 重连
    if result.is_err() {
        if let Some(actor) = actor_ref.upgrade() {
            let _ = actor.tell(ReconnectPrivate).await;
        }
    }

    result
}

// ============================================================================
// 消息构建和解析
// ============================================================================

fn build_subscribe_msg(kinds: &[SubscriptionKind]) -> String {
    let mut args: Vec<serde_json::Value> = Vec::new();

    for kind in kinds {
        match kind {
            SubscriptionKind::FundingRate { symbol } => {
                args.push(json!({
                    "channel": "funding-rate",
                    "instId": symbol.to_okx()
                }));
            }
            SubscriptionKind::BBO { symbol } => {
                args.push(json!({
                    "channel": "bbo-tbt",
                    "instId": symbol.to_okx()
                }));
            }
        }
    }

    if args.is_empty() {
        return String::new();
    }

    json!({
        "op": "subscribe",
        "args": args
    })
    .to_string()
}

fn build_unsubscribe_msg(kinds: &[SubscriptionKind]) -> String {
    let mut args: Vec<serde_json::Value> = Vec::new();

    for kind in kinds {
        match kind {
            SubscriptionKind::FundingRate { symbol } => {
                args.push(json!({
                    "channel": "funding-rate",
                    "instId": symbol.to_okx()
                }));
            }
            SubscriptionKind::BBO { symbol } => {
                args.push(json!({
                    "channel": "bbo-tbt",
                    "instId": symbol.to_okx()
                }));
            }
        }
    }

    if args.is_empty() {
        return String::new();
    }

    json!({
        "op": "unsubscribe",
        "args": args
    })
    .to_string()
}

fn parse_message(
    raw: &str,
    timestamp: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Option<ExchangeEvent> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;

    // 检查是否是事件响应（控制消息，返回 None）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => return None,
            "error" => {
                tracing::error!(raw = %raw, "OKX error");
                return None;
            }
            _ => return None,
        }
    }

    // 获取频道
    let channel = value
        .get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())?;

    match channel {
        "funding-rate" => {
            let push: WsPush<FundingRateData> = serde_json::from_str(raw).ok()?;
            let data = push.data.first()?;
            let rate = data.to_funding_rate()?;
            let symbol = rate.symbol.clone();
            Some(ExchangeEvent::FundingRateUpdate {
                symbol,
                exchange: Exchange::OKX,
                rate,
                timestamp,
            })
        }
        "bbo-tbt" => {
            let push: WsPush<BboData> = serde_json::from_str(raw).ok()?;
            let inst_id = push.arg.inst_id.as_ref()?;
            let data = push.data.first()?;
            let bbo = data.to_bbo(inst_id)?;
            let symbol = bbo.symbol.clone();
            Some(ExchangeEvent::BBOUpdate {
                symbol,
                exchange: Exchange::OKX,
                bbo,
                timestamp,
            })
        }
        "positions" => {
            let push: WsPush<PositionData> = serde_json::from_str(raw).ok()?;
            let data = push.data.first()?;
            let mut position = data.to_position()?;
            let symbol = position.symbol.clone();
            // qty 归一化: 张 -> 币
            if let Some(meta) = symbol_metas.get(&symbol) {
                position.size = meta.qty_to_coin(position.size);
            }
            Some(ExchangeEvent::PositionUpdate {
                symbol,
                exchange: Exchange::OKX,
                position,
                timestamp,
            })
        }
        "account" => {
            let push: WsPush<AccountData> = serde_json::from_str(raw).ok()?;
            let data = push.data.first()?;
            let equity = data.to_equity()?;
            Some(ExchangeEvent::EquityUpdate {
                exchange: Exchange::OKX,
                equity,
                timestamp,
            })
        }
        "orders" => {
            let push: WsPush<OrderPushData> = serde_json::from_str(raw).ok()?;
            let data = push.data.first()?;
            let update = data.to_order_update()?;
            let symbol = update.symbol.clone();
            Some(ExchangeEvent::OrderStatusUpdate {
                symbol,
                exchange: Exchange::OKX,
                update,
                timestamp,
            })
        }
        _ => None,
    }
}

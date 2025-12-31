//! Binance WebSocket Actor
//!
//! 处理 Binance 的公共和私有 WebSocket 连接

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::messaging::{IncomeEvent, ExchangeEventData};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use serde_json::json;
use std::collections::HashSet;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";

/// Private WebSocket URL 基础
const WS_PRIVATE_BASE: &str = "wss://fstream.binance.com/ws";

/// ListenKey 刷新间隔 (30 分钟)
const LISTEN_KEY_REFRESH_INTERVAL_SECS: u64 = 30 * 60;

// ============================================================================
// WebSocket 连接
// ============================================================================

/// WebSocket 连接状态
struct WsConnection {
    /// 发送消息的 channel
    tx: mpsc::Sender<String>,
    /// ws_loop 任务句柄
    _handle: JoinHandle<()>,
}

// ============================================================================
// BinanceActor
// ============================================================================

/// Binance Actor 参数
pub struct BinanceActorArgs {
    /// 凭证（可选）
    pub credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// REST 基础 URL（用于 ListenKey）
    pub rest_base_url: String,
}

/// Binance 凭证
#[derive(Clone)]
pub struct BinanceCredentials {
    pub api_key: String,
    pub secret: String,
}

/// Binance Actor
pub struct BinanceActor {
    /// 凭证
    credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
    /// REST 基础 URL
    rest_base_url: String,

    // 连接管理
    /// Public 连接
    public_conn: Option<WsConnection>,
    /// Private 连接
    private_conn: Option<WsConnection>,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,

    // Binance 特有
    /// ListenKey 刷新任务
    listen_key_refresh_handle: Option<JoinHandle<()>>,

    /// 自身弱引用
    self_ref: Option<WeakActorRef<Self>>,
}

impl BinanceActor {
    pub fn new(args: BinanceActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: args.event_sink,
            rest_base_url: args.rest_base_url,
            public_conn: None,
            private_conn: None,
            subscribed: HashSet::new(),
            listen_key_refresh_handle: None,
            self_ref: None,
        }
    }

    /// 创建 public WebSocket 连接
    async fn create_public_connection(&mut self) -> Result<(), WsError> {
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

        let handle = tokio::spawn(run_ws_loop(read, write, rx, self_ref, false));

        self.public_conn = Some(WsConnection { tx, _handle: handle });
        Ok(())
    }

    /// 创建 private WebSocket 连接
    async fn create_private_connection(&mut self) -> Result<(), WsError> {
        let credentials = self
            .credentials
            .as_ref()
            .ok_or_else(|| WsError::AuthFailed("No credentials".to_string()))?;

        // 获取 ListenKey (通过 REST API)
        let listen_key = create_listen_key(&self.rest_base_url, &credentials.api_key).await?;

        let url = format!("{}/{}", WS_PRIVATE_BASE, listen_key);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();
        let (tx, rx) = mpsc::channel::<String>(100);

        let self_ref = self
            .self_ref
            .as_ref()
            .ok_or_else(|| WsError::ConnectionFailed("Actor not started".to_string()))?
            .clone();

        let handle = tokio::spawn(run_ws_loop(read, write, rx, self_ref, true));

        self.private_conn = Some(WsConnection { tx, _handle: handle });

        // 启动 ListenKey 刷新任务
        let api_key = credentials.api_key.clone();
        let rest_base_url = self.rest_base_url.clone();
        self.listen_key_refresh_handle = Some(tokio::spawn(async move {
            listen_key_refresh_loop(&rest_base_url, &api_key).await;
        }));

        Ok(())
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) {
        let stream = kind_to_stream(kind);
        let msg = json!({
            "method": "SUBSCRIBE",
            "params": [stream],
            "id": 1
        })
        .to_string();

        if let Some(conn) = &self.public_conn {
            let _ = conn.tx.send(msg).await;
        }
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) {
        let stream = kind_to_stream(kind);
        let msg = json!({
            "method": "UNSUBSCRIBE",
            "params": [stream],
            "id": 2
        })
        .to_string();

        if let Some(conn) = &self.public_conn {
            let _ = conn.tx.send(msg).await;
        }
    }
}

impl Actor for BinanceActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.downgrade());

        // 1. 创建 public 连接
        self.create_public_connection().await?;

        // 2. 如果有凭证，创建 private 连接
        if self.credentials.is_some() {
            self.create_private_connection().await?;
        }

        tracing::info!(
            exchange = "Binance",
            has_private = self.private_conn.is_some(),
            "BinanceActor started"
        );

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // 取消 ListenKey 刷新任务
        if let Some(handle) = self.listen_key_refresh_handle.take() {
            handle.abort();
        }

        // 中止 public 连接的任务
        if let Some(conn) = self.public_conn.take() {
            conn._handle.abort();
        }

        // 中止 private 连接的任务
        if let Some(conn) = self.private_conn.take() {
            conn._handle.abort();
        }

        tracing::info!("BinanceActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查是否已订阅
        if self.subscribed.contains(&msg.kind) {
            return;
        }

        self.send_subscribe(&msg.kind).await;
        self.subscribed.insert(msg.kind);
    }
}

impl Message<Unsubscribe> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查是否已订阅
        if !self.subscribed.remove(&msg.kind) {
            return;
        }

        self.send_unsubscribe(&msg.kind).await;
    }
}

/// 内部 WebSocket 数据消息
pub struct WsData {
    pub data: String,
}

/// WebSocket 连接断开消息 (触发 Actor 停止)
struct WsDisconnected {
    error: WsError,
    is_private: bool,
}

impl Message<WsDisconnected> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: WsDisconnected,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let conn_type = if msg.is_private { "private" } else { "public" };
        tracing::error!(
            error = %msg.error,
            conn_type,
            "Binance WebSocket disconnected, stopping actor"
        );
        ctx.actor_ref().stop_gracefully().await.ok();
    }
}

impl Message<WsData> for BinanceActor {
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
                tracing::error!(error = %e, raw = %msg.data, "Failed to parse Binance message");
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
    actor_ref: WeakActorRef<BinanceActor>,
    is_private: bool,
) {
    let result = run_ws_loop_inner(&mut read, &mut write, &mut rx, &actor_ref).await;

    // 出错时通知 Actor 停止
    if let Err(e) = result {
        if let Some(actor) = actor_ref.upgrade() {
            let _ = actor.tell(WsDisconnected { error: e, is_private }).await;
        }
    }
}

async fn run_ws_loop_inner(
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin
              + Send),
    write: &mut (impl SinkExt<WsMessage> + Unpin + Send),
    rx: &mut mpsc::Receiver<String>,
    actor_ref: &WeakActorRef<BinanceActor>,
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

// ============================================================================
// ListenKey 管理
// ============================================================================

async fn create_listen_key(rest_base_url: &str, api_key: &str) -> Result<String, WsError> {
    #[derive(serde::Deserialize)]
    struct Response {
        #[serde(rename = "listenKey")]
        listen_key: String,
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/fapi/v1/listenKey", rest_base_url))
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await
        .map_err(|e| WsError::AuthFailed(e.to_string()))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(WsError::AuthFailed(format!(
            "Failed to create listen key: {}",
            text
        )));
    }

    let data: Response = resp
        .json()
        .await
        .map_err(|e| WsError::AuthFailed(e.to_string()))?;

    Ok(data.listen_key)
}

async fn listen_key_refresh_loop(rest_base_url: &str, api_key: &str) {
    let client = reqwest::Client::new();
    let interval = std::time::Duration::from_secs(LISTEN_KEY_REFRESH_INTERVAL_SECS);

    loop {
        tokio::time::sleep(interval).await;

        let result = client
            .put(format!("{}/fapi/v1/listenKey", rest_base_url))
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!("Binance ListenKey refreshed");
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(error = %text, "Failed to refresh Binance ListenKey");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to refresh Binance ListenKey");
            }
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

fn kind_to_stream(kind: &SubscriptionKind) -> String {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            format!("{}@markPrice@1s", symbol.to_binance().to_lowercase())
        }
        SubscriptionKind::BBO { symbol } => {
            format!("{}@bookTicker", symbol.to_binance().to_lowercase())
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

    // 检查是否是订阅响应（控制消息，返回空 Vec）
    if value.get("id").is_some() {
        if let Ok(resp) = serde_json::from_str::<WsResponse>(raw) {
            if let Some(err) = resp.error {
                return Err(WsError::ParseError(format!(
                    "Subscribe error: code={}, msg={}",
                    err.code, err.msg
                )));
            }
        }
        return Ok(Vec::new());
    }

    // 提取交易所事件时间 (E 字段，毫秒)
    let exchange_ts = value
        .get("E")
        .and_then(|v| v.as_u64())
        .unwrap_or(local_ts);

    // 根据事件类型解析
    let event_type = value
        .get("e")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing event type: {}", raw)))?;

    match event_type {
        "markPriceUpdate" => {
            let update: MarkPriceUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("markPriceUpdate parse: {}", e)))?;
            let rate = update
                .to_funding_rate(8.0)
                .ok_or_else(|| WsError::ParseError("Invalid funding rate".into()))?;
            Ok(vec![IncomeEvent {
                exchange_ts,
                local_ts,
                data: ExchangeEventData::FundingRate(rate),
            }])
        }
        "bookTicker" => {
            let ticker: BookTicker = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bookTicker parse: {}", e)))?;
            let bbo = ticker
                .to_bbo()
                .ok_or_else(|| WsError::ParseError("Invalid BBO data".into()))?;
            Ok(vec![IncomeEvent {
                exchange_ts,
                local_ts,
                data: ExchangeEventData::BBO(bbo),
            }])
        }
        "ACCOUNT_UPDATE" => {
            let update: AccountUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("ACCOUNT_UPDATE parse: {}", e)))?;

            let mut events = Vec::new();

            // 处理所有 position 更新
            for pos_data in &update.a.positions {
                if let Some(mut position) = pos_data.to_position() {
                    // qty 归一化: 张 -> 币
                    let meta = symbol_metas
                        .get(&position.symbol)
                        .expect("SymbolMeta not found for position symbol");
                    position.size = meta.qty_to_coin(position.size);
                    events.push(IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::Position(position),
                    });
                }
            }

            // 处理所有 balance 更新
            for bal_data in &update.a.balances {
                if let Some(balance) = bal_data.to_balance() {
                    events.push(IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::Balance(balance),
                    });
                }
            }

            Ok(events)
        }
        "ORDER_TRADE_UPDATE" => {
            let update: OrderTradeUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("ORDER_TRADE_UPDATE parse: {}", e)))?;
            let order_update = update
                .to_order_update()
                .ok_or_else(|| WsError::ParseError("Invalid order update".into()))?;
            Ok(vec![IncomeEvent {
                exchange_ts,
                local_ts,
                data: ExchangeEventData::OrderUpdate(order_update),
            }])
        }
        _ => {
            // 未知事件类型，记录警告但不报错
            tracing::warn!(event_type, raw, "Unknown Binance event type");
            Ok(Vec::new())
        }
    }
}

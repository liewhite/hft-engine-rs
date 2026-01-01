//! Binance WebSocket Actor
//!
//! 处理 Binance 的公共和私有 WebSocket 连接

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";

/// Private WebSocket URL 基础
const WS_PRIVATE_BASE: &str = "wss://fstream.binance.com/ws";

/// ListenKey 刷新间隔 (30 分钟)
const LISTEN_KEY_REFRESH_INTERVAL_SECS: u64 = 30 * 60;

// ============================================================================
// 协程退出信号
// ============================================================================

/// 协程退出信号
#[derive(Debug)]
enum TaskExit {
    PublicWs(WsError),
    PrivateWs(WsError),
    ListenKeyRefresh(WsError),
}

// ============================================================================
// WebSocket 连接
// ============================================================================

/// WebSocket 连接状态
struct WsConnection {
    /// 发送消息的 channel
    tx: mpsc::Sender<String>,
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

    /// 协程退出信号发送器
    task_exit_tx: Option<mpsc::Sender<TaskExit>>,
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
            task_exit_tx: None,
        }
    }

    /// 创建 public WebSocket 连接
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
        tokio::spawn(run_ws_loop(
            read,
            write,
            rx,
            weak_ref,
            task_exit_tx,
            false,
        ));

        self.public_conn = Some(WsConnection { tx });
        Ok(())
    }

    /// 创建 private WebSocket 连接
    async fn create_private_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
        task_exit_tx: mpsc::Sender<TaskExit>,
    ) -> Result<(), WsError> {
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

        let weak_ref = actor_ref.downgrade();
        let ws_exit_tx = task_exit_tx.clone();
        tokio::spawn(run_ws_loop(
            read,
            write,
            rx,
            weak_ref.clone(),
            ws_exit_tx,
            true,
        ));

        self.private_conn = Some(WsConnection { tx });

        // 启动 ListenKey 刷新任务
        let api_key = credentials.api_key.clone();
        let rest_base_url = self.rest_base_url.clone();
        tokio::spawn(listen_key_refresh_loop(
            rest_base_url,
            api_key,
            weak_ref,
            task_exit_tx,
        ));

        Ok(())
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let stream = kind_to_stream(kind);
        let msg = json!({
            "method": "SUBSCRIBE",
            "params": [stream],
            "id": 1
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

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let stream = kind_to_stream(kind);
        let msg = json!({
            "method": "UNSUBSCRIBE",
            "params": [stream],
            "id": 2
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

impl Actor for BinanceActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceActor"
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
        // Drop task_exit_tx 会导致 stream 结束
        self.task_exit_tx.take();
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
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查是否已订阅
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

impl Message<Unsubscribe> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查是否已订阅
        if !self.subscribed.remove(&msg.kind) {
            return;
        }

        if let Err(e) = self.send_unsubscribe(&msg.kind).await {
            tracing::error!(error = %e, "Failed to send unsubscribe, killing actor");
            ctx.actor_ref().kill();
        }
    }
}

/// 内部 WebSocket 数据消息
pub struct WsData {
    pub data: String,
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

/// 协程退出信号处理
impl Message<StreamMessage<TaskExit, (), ()>> for BinanceActor {
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
                    TaskExit::ListenKeyRefresh(e) => {
                        tracing::error!(error = %e, "ListenKey refresh loop exited, killing actor");
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
    actor_ref: WeakActorRef<BinanceActor>,
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

async fn listen_key_refresh_loop(
    rest_base_url: String,
    api_key: String,
    actor_ref: WeakActorRef<BinanceActor>,
    task_exit_tx: mpsc::Sender<TaskExit>,
) {
    let client = reqwest::Client::new();
    let interval = std::time::Duration::from_secs(LISTEN_KEY_REFRESH_INTERVAL_SECS);

    loop {
        tokio::time::sleep(interval).await;

        // Actor 已停止，退出协程
        if actor_ref.upgrade().is_none() {
            return;
        }

        let result = client
            .put(format!("{}/fapi/v1/listenKey", rest_base_url))
            .header("X-MBX-APIKEY", &api_key)
            .send()
            .await;

        let error = match result {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!("Binance ListenKey refreshed");
                continue;
            }
            Ok(resp) => {
                let text = resp.text().await.unwrap_or_default();
                format!("HTTP error: {}", text)
            }
            Err(e) => e.to_string(),
        };

        tracing::error!(error = %error, "ListenKey refresh failed");
        let _ = task_exit_tx
            .send(TaskExit::ListenKeyRefresh(WsError::AuthFailed(format!(
                "ListenKey refresh failed: {}",
                error
            ))))
            .await;
        return;
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
            let rate = update.to_funding_rate(8.0);
            Ok(vec![IncomeEvent {
                exchange_ts,
                local_ts,
                data: ExchangeEventData::FundingRate(rate),
            }])
        }
        "bookTicker" => {
            let ticker: BookTicker = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bookTicker parse: {}", e)))?;
            let bbo = ticker.to_bbo();
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
                let mut position = pos_data.to_position();
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

            // 处理所有 balance 更新
            for bal_data in &update.a.balances {
                let balance = bal_data.to_balance();
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::Balance(balance),
                });
            }

            Ok(events)
        }
        "ORDER_TRADE_UPDATE" => {
            let update: OrderTradeUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("ORDER_TRADE_UPDATE parse: {}", e)))?;
            let order_update = update.to_order_update();
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

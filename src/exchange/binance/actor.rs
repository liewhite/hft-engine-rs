//! Binance WebSocket Actor
//!
//! 处理 Binance 的公共和私有 WebSocket 连接

use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::client::{
    MarketData, MarketDataSink, ParsedMessage, Subscribe, SubscriptionKind, Unsubscribe, WsError,
};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 单个 WebSocket 连接的最大订阅数
const MAX_SUBSCRIPTIONS_PER_CONN: usize = 200;

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
    /// 订阅数量
    subscription_count: usize,
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
    /// 数据接收器
    pub data_sink: Arc<dyn MarketDataSink>,
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
    /// 数据接收器
    data_sink: Arc<dyn MarketDataSink>,
    /// REST 基础 URL
    rest_base_url: String,

    // 连接管理
    /// Public 连接列表
    public_conns: Vec<WsConnection>,
    /// Private 连接
    private_conn: Option<WsConnection>,
    /// 订阅到连接的映射 (kind -> public_conn index)
    subscriptions: HashMap<SubscriptionKind, usize>,

    // Binance 特有
    /// ListenKey
    listen_key: Option<String>,
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
            data_sink: args.data_sink,
            rest_base_url: args.rest_base_url,
            public_conns: Vec::new(),
            private_conn: None,
            subscriptions: HashMap::new(),
            listen_key: None,
            listen_key_refresh_handle: None,
            self_ref: None,
        }
    }

    /// 创建 public WebSocket 连接
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

        // 启动 ws_loop
        let handle = tokio::spawn(async move {
            if let Err(e) = run_public_ws_loop(read, write, rx, self_ref).await {
                tracing::warn!(error = %e, "Binance public ws_loop ended with error");
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

        // 启动 ws_loop
        let handle = tokio::spawn(async move {
            if let Err(e) = run_private_ws_loop(read, write, rx, self_ref).await {
                tracing::warn!(error = %e, "Binance private ws_loop ended with error");
            }
        });

        self.private_conn = Some(WsConnection {
            tx,
            subscription_count: 0,
            _handle: handle,
        });

        // 启动 ListenKey 刷新任务
        let api_key = credentials.api_key.clone();
        let rest_base_url = self.rest_base_url.clone();
        self.listen_key = Some(listen_key);
        self.listen_key_refresh_handle = Some(tokio::spawn(async move {
            listen_key_refresh_loop(&rest_base_url, &api_key).await;
        }));

        Ok(())
    }

    /// 查找或创建有空位的 public 连接
    async fn ensure_public_connection(&mut self) -> Result<usize, WsError> {
        // 查找有空位的连接
        for (idx, conn) in self.public_conns.iter().enumerate() {
            if conn.subscription_count < MAX_SUBSCRIPTIONS_PER_CONN {
                return Ok(idx);
            }
        }

        // 创建新连接
        self.create_public_connection().await
    }

    /// 发送订阅消息
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

    /// 发送取消订阅消息
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
}

impl Actor for BinanceActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceActor"
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
        if self.subscriptions.contains_key(&msg.kind) {
            return;
        }

        // 获取或创建连接
        let conn_idx = match self.ensure_public_connection().await {
            Ok(idx) => idx,
            Err(e) => {
                tracing::error!(error = %e, "Failed to ensure public connection");
                return;
            }
        };

        // 发送订阅
        self.send_subscribe(conn_idx, &msg.kind).await;
        self.subscriptions.insert(msg.kind, conn_idx);
    }
}

impl Message<Unsubscribe> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 查找订阅
        let conn_idx = match self.subscriptions.remove(&msg.kind) {
            Some(idx) => idx,
            None => return,
        };

        // 发送取消订阅
        self.send_unsubscribe(conn_idx, &msg.kind).await;
    }
}

/// 内部 WebSocket 数据消息
pub struct WsData {
    pub data: String,
}

impl Message<WsData> for BinanceActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        // 解析消息
        let parsed = match parse_message(&msg.data) {
            Some(p) => p,
            None => return,
        };

        // 转换为 MarketData 并发送
        let market_data = match parsed {
            ParsedMessage::FundingRate { symbol, rate } => MarketData::FundingRate {
                exchange: Exchange::Binance,
                symbol,
                rate,
            },
            ParsedMessage::BBO { symbol, bbo } => MarketData::BBO {
                exchange: Exchange::Binance,
                symbol,
                bbo,
            },
            ParsedMessage::Position { symbol, mut position } => {
                // qty 归一化: 张 -> 币
                if let Some(meta) = self.symbol_metas.get(&symbol) {
                    position.size = meta.qty_to_coin(position.size);
                }
                MarketData::Position {
                    exchange: Exchange::Binance,
                    symbol,
                    position,
                }
            }
            ParsedMessage::Balance(balance) => MarketData::Balance {
                exchange: Exchange::Binance,
                balance,
            },
            ParsedMessage::OrderUpdate { symbol, update } => MarketData::OrderUpdate {
                exchange: Exchange::Binance,
                symbol,
                update,
            },
            ParsedMessage::Equity(value) => MarketData::Equity {
                exchange: Exchange::Binance,
                value,
            },
            ParsedMessage::Subscribed | ParsedMessage::Pong | ParsedMessage::Ignored => return,
        };

        self.data_sink.send_market_data(market_data).await;
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
    actor_ref: WeakActorRef<BinanceActor>,
) -> Result<(), WsError> {
    loop {
        tokio::select! {
            // 发送消息
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

            // 接收消息
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
    actor_ref: WeakActorRef<BinanceActor>,
) -> Result<(), WsError> {
    loop {
        tokio::select! {
            // 发送消息
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

            // 接收消息
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
// 消息构建和解析
// ============================================================================

fn build_subscribe_msg(kinds: &[SubscriptionKind]) -> String {
    let mut streams: Vec<String> = Vec::new();

    for kind in kinds {
        match kind {
            SubscriptionKind::FundingRate { symbol } => {
                let s = symbol.to_binance().to_lowercase();
                streams.push(format!("{}@markPrice@1s", s));
            }
            SubscriptionKind::BBO { symbol } => {
                let s = symbol.to_binance().to_lowercase();
                streams.push(format!("{}@bookTicker", s));
            }
        }
    }

    if streams.is_empty() {
        return String::new();
    }

    json!({
        "method": "SUBSCRIBE",
        "params": streams,
        "id": 1
    })
    .to_string()
}

fn build_unsubscribe_msg(kinds: &[SubscriptionKind]) -> String {
    let mut streams: Vec<String> = Vec::new();

    for kind in kinds {
        match kind {
            SubscriptionKind::FundingRate { symbol } => {
                let s = symbol.to_binance().to_lowercase();
                streams.push(format!("{}@markPrice@1s", s));
            }
            SubscriptionKind::BBO { symbol } => {
                let s = symbol.to_binance().to_lowercase();
                streams.push(format!("{}@bookTicker", s));
            }
        }
    }

    if streams.is_empty() {
        return String::new();
    }

    json!({
        "method": "UNSUBSCRIBE",
        "params": streams,
        "id": 2
    })
    .to_string()
}

fn parse_message(raw: &str) -> Option<ParsedMessage> {
    // 尝试解析为 JSON
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;

    // 检查是否是订阅响应
    if value.get("id").is_some() {
        if let Ok(resp) = serde_json::from_str::<WsResponse>(raw) {
            if resp.error.is_some() {
                tracing::error!(raw = %raw, "Binance subscribe error");
            }
            return Some(ParsedMessage::Subscribed);
        }
    }

    // 根据事件类型解析
    let event_type = value.get("e")?.as_str()?;

    match event_type {
        "markPriceUpdate" => {
            let update: MarkPriceUpdate = serde_json::from_str(raw).ok()?;
            let symbol = update.symbol()?;
            // 使用默认 8h 间隔
            let rate = update.to_funding_rate(8.0)?;
            Some(ParsedMessage::FundingRate { symbol, rate })
        }
        "bookTicker" => {
            let ticker: BookTicker = serde_json::from_str(raw).ok()?;
            let bbo = ticker.to_bbo()?;
            let symbol = bbo.symbol.clone();
            Some(ParsedMessage::BBO { symbol, bbo })
        }
        "ACCOUNT_UPDATE" => {
            let update: AccountUpdate = serde_json::from_str(raw).ok()?;

            // 返回第一个 position 更新 (简化处理)
            if let Some(pos_data) = update.a.positions.first() {
                if let Some(position) = pos_data.to_position() {
                    let symbol = position.symbol.clone();
                    return Some(ParsedMessage::Position { symbol, position });
                }
            }

            // 返回第一个 balance 更新
            if let Some(bal_data) = update.a.balances.first() {
                if let Some(balance) = bal_data.to_balance() {
                    return Some(ParsedMessage::Balance(balance));
                }
            }

            Some(ParsedMessage::Ignored)
        }
        "ORDER_TRADE_UPDATE" => {
            let update: OrderTradeUpdate = serde_json::from_str(raw).ok()?;
            let order_update = update.to_order_update()?;
            let symbol = order_update.symbol.clone();
            Some(ParsedMessage::OrderUpdate {
                symbol,
                update: order_update,
            })
        }
        _ => Some(ParsedMessage::Ignored),
    }
}

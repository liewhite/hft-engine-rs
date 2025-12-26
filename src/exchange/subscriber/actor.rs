//! SubscriberActor 实现

use super::{
    ExchangeConfig, MarketData, ParsedMessage, Subscribe, SubscribeError, SubscriberArgs,
    SubscriptionKind, Unsubscribe,
};
use crate::domain::SymbolMeta;
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

/// 连接 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(u64);

/// 连接类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionType {
    Public,
    Private,
}

/// WebSocket 连接状态
struct Connection {
    conn_type: ConnectionType,
    subscriptions: HashSet<SubscriptionKind>,
    /// WebSocket 写端的 channel
    write_tx: mpsc::Sender<String>,
}

/// 单个 symbol 的 funding 状态 (用于动态计算结算间隔)
#[derive(Debug, Clone, Default)]
struct FundingState {
    /// 上次看到的 next_funding_time
    last_next_funding_time: u64,
    /// 计算出的间隔 (小时)
    interval_hours: f64,
}

impl FundingState {
    /// 更新状态并返回间隔
    fn update(&mut self, next_funding_time: u64) -> f64 {
        if self.last_next_funding_time == 0 {
            self.last_next_funding_time = next_funding_time;
            self.interval_hours = 8.0; // 默认 8 小时
            return self.interval_hours;
        }

        if next_funding_time != self.last_next_funding_time {
            let interval_ms = next_funding_time.saturating_sub(self.last_next_funding_time);
            self.interval_hours = Self::round_to_hour(interval_ms);
            self.last_next_funding_time = next_funding_time;
        }

        self.interval_hours
    }

    /// 将毫秒转换为小时并四舍五入到 0.5
    fn round_to_hour(interval_ms: u64) -> f64 {
        let hours = (interval_ms as f64) / (1000.0 * 60.0 * 60.0);
        (hours * 2.0).round() / 2.0
    }
}

/// SubscriberActor - 交易所 WebSocket 订阅器
pub struct SubscriberActor<C: ExchangeConfig> {
    /// Symbol 元数据 (用于 qty 归一化)
    symbol_metas: Arc<HashMap<crate::domain::Symbol, SymbolMeta>>,
    /// 认证凭证
    credentials: C::Credentials,
    /// 数据输出 channel
    data_sink: mpsc::Sender<MarketData>,

    /// 连接池
    connections: HashMap<ConnectionId, Connection>,
    /// 订阅到连接的映射
    subscription_to_conn: HashMap<SubscriptionKind, ConnectionId>,
    /// 下一个连接 ID
    next_conn_id: u64,

    /// Funding 状态追踪 (用于动态计算结算间隔)
    funding_states: HashMap<crate::domain::Symbol, FundingState>,

    /// Actor 自身引用 (在 on_start 中设置)
    self_ref: Option<WeakActorRef<Self>>,

    _marker: std::marker::PhantomData<C>,
}

impl<C: ExchangeConfig> SubscriberActor<C> {
    /// 创建新的 SubscriberActor
    pub fn new(args: SubscriberArgs<C>) -> Self {
        Self {
            symbol_metas: args.symbol_metas,
            credentials: args.credentials,
            data_sink: args.data_sink,
            connections: HashMap::new(),
            subscription_to_conn: HashMap::new(),
            next_conn_id: 0,
            funding_states: HashMap::new(),
            self_ref: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// 分配新的连接 ID
    fn alloc_conn_id(&mut self) -> ConnectionId {
        let id = ConnectionId(self.next_conn_id);
        self.next_conn_id += 1;
        id
    }

    /// 查找或创建 public 连接
    async fn ensure_public_connection(&mut self) -> Result<ConnectionId, SubscribeError> {
        // 查找有空位的 public 连接
        for (id, conn) in &self.connections {
            if conn.conn_type == ConnectionType::Public
                && conn.subscriptions.len() < C::MAX_SUBSCRIPTIONS_PER_CONN
            {
                return Ok(*id);
            }
        }

        // 创建新的 public 连接
        self.create_connection(ConnectionType::Public, C::PUBLIC_WS_URL)
            .await
    }

    /// 确保 private 连接存在
    async fn ensure_private_connection(&mut self) -> Result<ConnectionId, SubscribeError> {
        // 查找已有的 private 连接
        for (id, conn) in &self.connections {
            if conn.conn_type == ConnectionType::Private {
                return Ok(*id);
            }
        }

        // 创建新的 private 连接
        self.create_connection(ConnectionType::Private, C::PRIVATE_WS_URL)
            .await
    }

    /// 创建 WebSocket 连接
    async fn create_connection(
        &mut self,
        conn_type: ConnectionType,
        url: &str,
    ) -> Result<ConnectionId, SubscribeError> {
        let conn_id = self.alloc_conn_id();
        let (write_tx, write_rx) = mpsc::channel::<String>(64);

        // 获取 actor ref
        let actor_ref = self
            .self_ref
            .as_ref()
            .and_then(|r| r.upgrade())
            .ok_or_else(|| SubscribeError::ConnectionFailed("Actor not initialized".into()))?;

        // 启动连接任务
        let url = url.to_string();
        let credentials = self.credentials.clone();
        let is_private = conn_type == ConnectionType::Private;

        tokio::spawn(async move {
            run_connection_loop::<C>(conn_id, url, credentials, is_private, write_rx, actor_ref)
                .await;
        });

        // 记录连接
        self.connections.insert(
            conn_id,
            Connection {
                conn_type,
                subscriptions: HashSet::new(),
                write_tx,
            },
        );

        Ok(conn_id)
    }

    /// 发送订阅消息到 WebSocket
    async fn send_subscribe(&mut self, conn_id: ConnectionId, kind: &SubscriptionKind) {
        if let Some(conn) = self.connections.get(&conn_id) {
            let msg = C::build_subscribe_msg(&[kind.clone()]);
            let _ = conn.write_tx.send(msg).await;
        }
    }

    /// 发送取消订阅消息到 WebSocket
    async fn send_unsubscribe(&mut self, conn_id: ConnectionId, kind: &SubscriptionKind) {
        if let Some(conn) = self.connections.get(&conn_id) {
            let msg = C::build_unsubscribe_msg(&[kind.clone()]);
            let _ = conn.write_tx.send(msg).await;
        }
    }

    /// 检查并关闭空闲连接
    fn maybe_close_connection(&mut self, conn_id: ConnectionId) {
        if let Some(conn) = self.connections.get(&conn_id) {
            if conn.subscriptions.is_empty() {
                // 连接没有订阅了，关闭它
                // drop write_tx 会导致连接任务退出
                self.connections.remove(&conn_id);
            }
        }
    }

    /// 处理解析后的消息，进行 qty 归一化并发送到 data_sink
    async fn handle_parsed_message(&mut self, msg: ParsedMessage) {
        let market_data = match msg {
            ParsedMessage::FundingRate {
                symbol,
                mut rate,
                next_funding_time,
            } => {
                // 如果有 next_funding_time，更新状态并设置正确的间隔
                if let Some(nft) = next_funding_time {
                    let state = self.funding_states.entry(symbol.clone()).or_default();
                    let interval = state.update(nft);
                    rate.settle_interval_hours = interval;
                }
                MarketData::FundingRate {
                    exchange: C::EXCHANGE,
                    symbol,
                    rate,
                }
            }
            ParsedMessage::BBO { symbol, bbo } => MarketData::BBO {
                exchange: C::EXCHANGE,
                symbol,
                bbo,
            },
            ParsedMessage::Position { symbol, mut position } => {
                // qty 归一化: 张 -> 币
                if let Some(meta) = self.symbol_metas.get(&symbol) {
                    position.size = meta.qty_to_coin(position.size);
                }
                MarketData::Position {
                    exchange: C::EXCHANGE,
                    symbol,
                    position,
                }
            }
            ParsedMessage::Balance(balance) => MarketData::Balance {
                exchange: C::EXCHANGE,
                balance,
            },
            ParsedMessage::OrderUpdate { symbol, update } => MarketData::OrderUpdate {
                exchange: C::EXCHANGE,
                symbol,
                update,
            },
            ParsedMessage::Equity(value) => MarketData::Equity {
                exchange: C::EXCHANGE,
                value,
            },
            ParsedMessage::Subscribed | ParsedMessage::Pong | ParsedMessage::Ignored => return,
        };

        let _ = self.data_sink.send(market_data).await;
    }
}

impl<C: ExchangeConfig> Actor for SubscriberActor<C> {
    type Mailbox = UnboundedMailbox<Self>;

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.downgrade());
        tracing::info!(exchange = %C::EXCHANGE, "SubscriberActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!(exchange = %C::EXCHANGE, "SubscriberActor stopped");
        Ok(())
    }
}

// === Message Handlers ===

impl<C: ExchangeConfig> Message<Subscribe> for SubscriberActor<C> {
    type Reply = Result<(), SubscribeError>;

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查是否已订阅
        if self.subscription_to_conn.contains_key(&msg.kind) {
            return Ok(());
        }

        // 惰性创建连接
        let conn_id = match &msg.kind {
            SubscriptionKind::FundingRate { .. } | SubscriptionKind::BBO { .. } => {
                self.ensure_public_connection().await?
            }
            SubscriptionKind::Private => self.ensure_private_connection().await?,
        };

        // 发送订阅消息
        self.send_subscribe(conn_id, &msg.kind).await;

        // 记录订阅
        if let Some(conn) = self.connections.get_mut(&conn_id) {
            conn.subscriptions.insert(msg.kind.clone());
        }
        self.subscription_to_conn.insert(msg.kind, conn_id);

        Ok(())
    }
}

impl<C: ExchangeConfig> Message<Unsubscribe> for SubscriberActor<C> {
    type Reply = ();

    async fn handle(&mut self, msg: Unsubscribe, _ctx: Context<'_, Self, Self::Reply>) {
        if let Some(conn_id) = self.subscription_to_conn.remove(&msg.kind) {
            // 发送取消订阅消息
            self.send_unsubscribe(conn_id, &msg.kind).await;

            // 从连接的订阅集合中移除
            if let Some(conn) = self.connections.get_mut(&conn_id) {
                conn.subscriptions.remove(&msg.kind);
            }

            // 检查是否需要关闭连接
            self.maybe_close_connection(conn_id);
        }
    }
}

/// 内部消息: WebSocket 收到数据
struct WsDataReceived {
    _conn_id: ConnectionId,
    data: String,
}

impl<C: ExchangeConfig> Message<WsDataReceived> for SubscriberActor<C> {
    type Reply = ();

    async fn handle(&mut self, msg: WsDataReceived, _ctx: Context<'_, Self, Self::Reply>) {
        if let Some(parsed) = C::parse_message(&msg.data) {
            self.handle_parsed_message(parsed).await;
        }
    }
}

/// 内部消息: 连接断开，需要重连
struct ConnectionLost {
    conn_id: ConnectionId,
}

impl<C: ExchangeConfig> Message<ConnectionLost> for SubscriberActor<C> {
    type Reply = ();

    async fn handle(&mut self, msg: ConnectionLost, _ctx: Context<'_, Self, Self::Reply>) {
        tracing::warn!(
            exchange = %C::EXCHANGE,
            conn_id = msg.conn_id.0,
            "Connection lost, will reconnect"
        );

        // 获取该连接的订阅列表
        let subscriptions: Vec<SubscriptionKind> =
            if let Some(conn) = self.connections.get(&msg.conn_id) {
                conn.subscriptions.iter().cloned().collect()
            } else {
                return;
            };

        // 移除旧连接
        self.connections.remove(&msg.conn_id);

        // 重新订阅 (会创建新连接)
        for kind in subscriptions {
            self.subscription_to_conn.remove(&kind);
            // 发送订阅消息给自己
            if let Some(actor_ref) = self.self_ref.as_ref().and_then(|r| r.upgrade()) {
                let _ = actor_ref.tell(Subscribe { kind }).await;
            }
        }
    }
}

// === Connection Loop ===

/// 运行 WebSocket 连接循环 (带重连)
async fn run_connection_loop<C: ExchangeConfig>(
    conn_id: ConnectionId,
    url: String,
    credentials: C::Credentials,
    is_private: bool,
    mut write_rx: mpsc::Receiver<String>,
    actor_ref: ActorRef<SubscriberActor<C>>,
) {
    loop {
        match connect_and_run::<C>(
            &url,
            &credentials,
            is_private,
            &mut write_rx,
            &actor_ref,
            conn_id,
        )
        .await
        {
            Ok(()) => {
                // 正常退出 (write_rx 关闭)
                tracing::info!(
                    exchange = %C::EXCHANGE,
                    conn_id = conn_id.0,
                    "Connection closed normally"
                );
                return;
            }
            Err(e) => {
                tracing::error!(
                    exchange = %C::EXCHANGE,
                    conn_id = conn_id.0,
                    error = %e,
                    "Connection error"
                );

                // 通知 actor 连接断开
                let _ = actor_ref.tell(ConnectionLost { conn_id }).await;

                // 退出循环，让 actor 处理重连
                return;
            }
        }
    }
}

/// 连接并运行消息循环
async fn connect_and_run<C: ExchangeConfig>(
    url: &str,
    credentials: &C::Credentials,
    is_private: bool,
    write_rx: &mut mpsc::Receiver<String>,
    actor_ref: &ActorRef<SubscriberActor<C>>,
    conn_id: ConnectionId,
) -> Result<(), String> {
    // 连接 WebSocket
    let (ws_stream, _) = connect_async(url)
        .await
        .map_err(|e| format!("Connect failed: {}", e))?;

    let (mut write, mut read) = ws_stream.split();

    // Private 连接需要认证
    if is_private {
        let auth_msg = C::build_auth_msg(credentials);
        write
            .send(WsMessage::Text(auth_msg))
            .await
            .map_err(|e| format!("Auth send failed: {}", e))?;

        // 等待认证响应 (简化处理，实际可能需要检查响应)
        // TODO: 检查认证响应
    }

    tracing::info!(
        exchange = %C::EXCHANGE,
        conn_id = conn_id.0,
        is_private = is_private,
        "WebSocket connected"
    );

    // 消息循环
    loop {
        tokio::select! {
            // 接收发送请求
            msg = write_rx.recv() => {
                match msg {
                    Some(text) => {
                        if let Err(e) = write.send(WsMessage::Text(text)).await {
                            return Err(format!("Send failed: {}", e));
                        }
                    }
                    None => {
                        // write_rx 关闭，正常退出
                        let _ = write.close().await;
                        return Ok(());
                    }
                }
            }

            // 接收 WebSocket 消息
            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        let _ = actor_ref.tell(WsDataReceived {
                            _conn_id: conn_id,
                            data: text,
                        }).await;
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        return Err("WebSocket closed by server".into());
                    }
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {}", e));
                    }
                    None => {
                        return Err("WebSocket stream ended".into());
                    }
                    _ => {}
                }
            }
        }
    }
}

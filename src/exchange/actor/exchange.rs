//! ExchangeActor - 管理单个交易所的所有 WebSocket 连接
//!
//! 职责：订阅管理、连接池管理、数据解析和归一化

use super::ws::{
    Connect, ConnectionId, ConnectionType, SendMessage, UpstreamEvent, WebSocketActor,
    WebSocketActorArgs,
};
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::subscriber::{
    ExchangeConfig, MarketData, ParsedMessage, Subscribe, SubscribeError, SubscriptionKind,
    Unsubscribe,
};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::request::MessageSend;
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

/// ExchangeActor 初始化参数
pub struct ExchangeActorArgs<C: ExchangeConfig> {
    /// Symbol 元数据 (用于 qty 归一化)
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 认证凭证
    pub credentials: C::Credentials,
    /// 数据输出 channel
    pub data_sink: mpsc::Sender<MarketData>,
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

/// ExchangeActor - 管理单个交易所的所有 WebSocket 连接
pub struct ExchangeActor<C: ExchangeConfig> {
    /// Symbol 元数据 (用于 qty 归一化)
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 认证凭证
    credentials: C::Credentials,
    /// 数据输出 channel
    data_sink: mpsc::Sender<MarketData>,

    /// WebSocket actors (ConnectionId -> ActorRef)
    ws_actors: HashMap<ConnectionId, ActorRef<WebSocketActor<C>>>,
    /// 连接的订阅集合
    conn_subscriptions: HashMap<ConnectionId, HashSet<SubscriptionKind>>,
    /// 连接类型
    conn_types: HashMap<ConnectionId, ConnectionType>,
    /// 订阅到连接的映射
    subscription_to_conn: HashMap<SubscriptionKind, ConnectionId>,
    /// 下一个连接 ID
    next_conn_id: u64,

    /// 上游事件通道 (WebSocketActor -> ExchangeActor)
    upstream_tx: mpsc::Sender<UpstreamEvent>,
    upstream_rx: Option<mpsc::Receiver<UpstreamEvent>>,

    /// Funding 状态追踪 (用于动态计算结算间隔)
    funding_states: HashMap<Symbol, FundingState>,

    /// Actor 自身引用
    self_ref: Option<WeakActorRef<Self>>,

    _marker: std::marker::PhantomData<C>,
}

impl<C: ExchangeConfig> ExchangeActor<C> {
    pub fn new(args: ExchangeActorArgs<C>) -> Self {
        let (upstream_tx, upstream_rx) = mpsc::channel(256);

        Self {
            symbol_metas: args.symbol_metas,
            credentials: args.credentials,
            data_sink: args.data_sink,
            ws_actors: HashMap::new(),
            conn_subscriptions: HashMap::new(),
            conn_types: HashMap::new(),
            subscription_to_conn: HashMap::new(),
            next_conn_id: 0,
            upstream_tx,
            upstream_rx: Some(upstream_rx),
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
        for (id, subs) in &self.conn_subscriptions {
            if self.conn_types.get(id) == Some(&ConnectionType::Public)
                && subs.len() < C::MAX_SUBSCRIPTIONS_PER_CONN
            {
                return Ok(*id);
            }
        }

        // 创建新的 public 连接
        self.create_ws_actor(ConnectionType::Public, C::PUBLIC_WS_URL)
            .await
    }

    /// 确保 private 连接存在
    async fn ensure_private_connection(&mut self) -> Result<ConnectionId, SubscribeError> {
        // 查找已有的 private 连接
        for (id, _) in &self.conn_subscriptions {
            if self.conn_types.get(id) == Some(&ConnectionType::Private) {
                return Ok(*id);
            }
        }

        // 创建新的 private 连接
        self.create_ws_actor(ConnectionType::Private, C::PRIVATE_WS_URL)
            .await
    }

    /// 创建 WebSocketActor
    async fn create_ws_actor(
        &mut self,
        conn_type: ConnectionType,
        url: &str,
    ) -> Result<ConnectionId, SubscribeError> {
        let conn_id = self.alloc_conn_id();

        // 创建 WebSocketActor
        let ws_args = WebSocketActorArgs {
            conn_id,
            conn_type,
            url: url.to_string(),
            credentials: self.credentials.clone(),
            upstream: self.upstream_tx.clone(),
        };

        let ws_actor = kameo::spawn(WebSocketActor::<C>::new(ws_args));

        // 发送 Connect 消息
        ws_actor
            .ask(Connect)
            .send()
            .await
            .map_err(|e| SubscribeError::ConnectionFailed(format!("Actor error: {}", e)))?;

        // 记录连接
        self.ws_actors.insert(conn_id, ws_actor);
        self.conn_subscriptions.insert(conn_id, HashSet::new());
        self.conn_types.insert(conn_id, conn_type);

        tracing::info!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            conn_type = ?conn_type,
            "WebSocketActor created"
        );

        Ok(conn_id)
    }

    /// 发送订阅消息到 WebSocket
    async fn send_subscribe(&self, conn_id: ConnectionId, kind: &SubscriptionKind) {
        if let Some(ws_actor) = self.ws_actors.get(&conn_id) {
            let msg = C::build_subscribe_msg(&[kind.clone()]);
            if !msg.is_empty() {
                let _ = ws_actor.tell(SendMessage(msg)).await;
            }
        }
    }

    /// 发送取消订阅消息到 WebSocket
    async fn send_unsubscribe(&self, conn_id: ConnectionId, kind: &SubscriptionKind) {
        if let Some(ws_actor) = self.ws_actors.get(&conn_id) {
            let msg = C::build_unsubscribe_msg(&[kind.clone()]);
            if !msg.is_empty() {
                let _ = ws_actor.tell(SendMessage(msg)).await;
            }
        }
    }

    /// 检查并关闭空闲连接
    async fn maybe_close_connection(&mut self, conn_id: ConnectionId) {
        if let Some(subs) = self.conn_subscriptions.get(&conn_id) {
            if subs.is_empty() {
                // 连接没有订阅了，关闭它
                if let Some(ws_actor) = self.ws_actors.remove(&conn_id) {
                    ws_actor.stop_gracefully().await.ok();
                }
                self.conn_subscriptions.remove(&conn_id);
                self.conn_types.remove(&conn_id);
            }
        }
    }

    /// 处理上游事件
    async fn handle_upstream_event(&mut self, event: UpstreamEvent) {
        match event {
            UpstreamEvent::Data { data, .. } => {
                if let Some(parsed) = C::parse_message(&data) {
                    self.handle_parsed_message(parsed).await;
                }
            }
            UpstreamEvent::Disconnected { conn_id, error } => {
                self.handle_disconnection(conn_id, error).await;
            }
        }
    }

    /// 处理连接断开
    async fn handle_disconnection(&mut self, conn_id: ConnectionId, error: Option<String>) {
        tracing::warn!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            error = ?error,
            "WebSocket connection lost, will reconnect"
        );

        // 获取该连接的订阅列表
        let subscriptions: Vec<SubscriptionKind> =
            if let Some(subs) = self.conn_subscriptions.get(&conn_id) {
                subs.iter().cloned().collect()
            } else {
                return;
            };

        // 移除旧连接
        if let Some(ws_actor) = self.ws_actors.remove(&conn_id) {
            ws_actor.stop_gracefully().await.ok();
        }
        self.conn_subscriptions.remove(&conn_id);
        self.conn_types.remove(&conn_id);

        // 重新订阅 (会创建新连接)
        for kind in subscriptions {
            self.subscription_to_conn.remove(&kind);
            // 发送订阅消息给自己
            if let Some(actor_ref) = self.self_ref.as_ref().and_then(|r| r.upgrade()) {
                let _ = actor_ref.tell(Subscribe { kind }).await;
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
            ParsedMessage::Position {
                symbol,
                mut position,
            } => {
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

impl<C: ExchangeConfig> Actor for ExchangeActor<C> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ExchangeActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.downgrade());

        // 启动上游事件处理任务
        let mut upstream_rx = self.upstream_rx.take().expect("upstream_rx already taken");
        let weak_ref = actor_ref.downgrade();

        tokio::spawn(async move {
            while let Some(event) = upstream_rx.recv().await {
                if let Some(actor) = weak_ref.upgrade() {
                    let _ = actor.tell(InternalUpstreamEvent(event)).await;
                } else {
                    break;
                }
            }
        });

        tracing::info!(exchange = %C::EXCHANGE, "ExchangeActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // 停止所有 WebSocketActor
        for (_, ws_actor) in self.ws_actors.drain() {
            ws_actor.stop_gracefully().await.ok();
        }
        tracing::info!(exchange = %C::EXCHANGE, "ExchangeActor stopped");
        Ok(())
    }
}

// === Message Handlers ===

impl<C: ExchangeConfig> Message<Subscribe> for ExchangeActor<C> {
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
        if let Some(subs) = self.conn_subscriptions.get_mut(&conn_id) {
            subs.insert(msg.kind.clone());
        }
        self.subscription_to_conn.insert(msg.kind, conn_id);

        Ok(())
    }
}

impl<C: ExchangeConfig> Message<Unsubscribe> for ExchangeActor<C> {
    type Reply = ();

    async fn handle(&mut self, msg: Unsubscribe, _ctx: Context<'_, Self, Self::Reply>) {
        if let Some(conn_id) = self.subscription_to_conn.remove(&msg.kind) {
            // 发送取消订阅消息
            self.send_unsubscribe(conn_id, &msg.kind).await;

            // 从连接的订阅集合中移除
            if let Some(subs) = self.conn_subscriptions.get_mut(&conn_id) {
                subs.remove(&msg.kind);
            }

            // 检查是否需要关闭连接
            self.maybe_close_connection(conn_id).await;
        }
    }
}

/// 内部消息: 上游事件
struct InternalUpstreamEvent(UpstreamEvent);

impl<C: ExchangeConfig> Message<InternalUpstreamEvent> for ExchangeActor<C> {
    type Reply = ();

    async fn handle(&mut self, msg: InternalUpstreamEvent, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_upstream_event(msg.0).await;
    }
}

//! ExchangeActor - 管理单个交易所的所有 WebSocket 连接
//!
//! 职责：订阅管理、连接池管理、数据解析和归一化
//!
//! Supervisor 职责：
//! - 使用 spawn_link 创建 WebSocketActor
//! - on_link_died 时根据错误类型决定重启或级联 die

use super::private_ws::PrivateConnectionHandle;
use super::public_ws::{
    ConnectionId, PublicWsActor, PublicWsActorArgs, SendMessage, WsData, WsDataSink,
};
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::subscriber::{
    ExchangeWsProtocol, MarketData, ParsedMessage, Subscribe, SubscribeError, SubscriptionKind,
    Unsubscribe,
};
use kameo::actor::spawn_link;
use kameo::actor::{ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// MarketData 接收器 trait (用于类型擦除)
#[async_trait::async_trait]
pub trait MarketDataSink: Send + Sync + 'static {
    async fn send_market_data(&self, data: MarketData);
}

/// ExchangeActor 初始化参数
pub struct ExchangeActorArgs<C: ExchangeWsProtocol, S: MarketDataSink> {
    /// Symbol 元数据 (用于 qty 归一化)
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 认证凭证
    pub credentials: C::Credentials,
    /// REST 客户端
    pub rest_client: Arc<C::RestClient>,
    /// 数据接收器 (父 Actor)
    pub data_sink: Arc<S>,
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

/// 连接类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionType {
    Public,
    Private,
}

/// 连接信息 (用于重启)
#[derive(Debug, Clone)]
struct ConnectionInfo {
    conn_type: ConnectionType,
    subscriptions: HashSet<SubscriptionKind>,
}

/// ExchangeActor 的数据接收器 (实现 WsDataSink)
struct ExchangeDataSink<C: ExchangeWsProtocol, S: MarketDataSink> {
    actor_ref: WeakActorRef<ExchangeActor<C, S>>,
}

#[async_trait::async_trait]
impl<C: ExchangeWsProtocol, S: MarketDataSink> WsDataSink for ExchangeDataSink<C, S> {
    async fn send_data(&self, data: WsData) {
        if let Some(actor) = self.actor_ref.upgrade() {
            let _ = actor.tell(InternalWsData(data)).await;
        }
    }
}

/// ExchangeActor - 管理单个交易所的所有 WebSocket 连接
pub struct ExchangeActor<C: ExchangeWsProtocol, S: MarketDataSink> {
    /// Symbol 元数据 (用于 qty 归一化)
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 认证凭证
    credentials: C::Credentials,
    /// REST 客户端
    rest_client: Arc<C::RestClient>,
    /// 数据接收器 (父 Actor)
    data_sink: Arc<S>,

    /// Public WebSocket actors (ConnectionId -> ActorRef)
    public_actors: HashMap<ConnectionId, ActorRef<PublicWsActor<ExchangeDataSink<C, S>>>>,
    /// Private connection handle (最多一个)
    private_handle: Option<Box<dyn PrivateConnectionHandle>>,
    /// ActorID -> ConnectionId 映射 (用于 on_link_died)
    actor_to_conn: HashMap<ActorID, ConnectionId>,
    /// 连接信息 (用于重启)
    conn_info: HashMap<ConnectionId, ConnectionInfo>,
    /// 订阅到连接的映射
    subscription_to_conn: HashMap<SubscriptionKind, ConnectionId>,
    /// 下一个连接 ID
    next_conn_id: u64,

    /// Funding 状态追踪 (用于动态计算结算间隔)
    funding_states: HashMap<Symbol, FundingState>,

    /// 重连退避状态 (ConnectionId -> 当前退避时间毫秒)
    reconnect_backoff: HashMap<ConnectionId, u64>,

    /// Actor 自身引用
    self_ref: Option<ActorRef<Self>>,

    _marker: std::marker::PhantomData<C>,
}

/// 重连退避常量
const RECONNECT_BACKOFF_INITIAL_MS: u64 = 1000; // 1 秒
const RECONNECT_BACKOFF_MAX_MS: u64 = 60_000; // 60 秒
const RECONNECT_BACKOFF_MULTIPLIER: u64 = 2;

impl<C: ExchangeWsProtocol, S: MarketDataSink> ExchangeActor<C, S> {
    pub fn new(args: ExchangeActorArgs<C, S>) -> Self {
        Self {
            symbol_metas: args.symbol_metas,
            credentials: args.credentials,
            rest_client: args.rest_client,
            data_sink: args.data_sink,
            public_actors: HashMap::new(),
            private_handle: None,
            actor_to_conn: HashMap::new(),
            conn_info: HashMap::new(),
            subscription_to_conn: HashMap::new(),
            next_conn_id: 0,
            funding_states: HashMap::new(),
            reconnect_backoff: HashMap::new(),
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

    /// 获取并增加退避时间
    fn get_and_increase_backoff(&mut self, conn_id: ConnectionId) -> Duration {
        let current = self
            .reconnect_backoff
            .get(&conn_id)
            .copied()
            .unwrap_or(RECONNECT_BACKOFF_INITIAL_MS);

        let next = (current * RECONNECT_BACKOFF_MULTIPLIER).min(RECONNECT_BACKOFF_MAX_MS);
        self.reconnect_backoff.insert(conn_id, next);

        Duration::from_millis(current)
    }

    /// 重置退避时间
    fn reset_backoff(&mut self, conn_id: ConnectionId) {
        self.reconnect_backoff.remove(&conn_id);
    }

    /// 查找或创建 public 连接
    async fn ensure_public_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
    ) -> Result<ConnectionId, SubscribeError> {
        // 查找有空位的 public 连接
        for (id, info) in &self.conn_info {
            if info.conn_type == ConnectionType::Public
                && info.subscriptions.len() < C::MAX_SUBSCRIPTIONS_PER_CONN
            {
                return Ok(*id);
            }
        }

        // 创建新的 public 连接
        self.create_public_actor(actor_ref).await
    }

    /// 确保 private 连接存在
    async fn ensure_private_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
    ) -> Result<ConnectionId, SubscribeError> {
        // 如果已有 private 连接，返回其 conn_id
        if let Some(handle) = &self.private_handle {
            return Ok(handle.conn_id());
        }

        // 创建新的 private 连接
        self.create_private_connection(actor_ref).await
    }

    /// 使用 spawn_link 创建 PublicWsActor
    async fn create_public_actor(
        &mut self,
        actor_ref: &ActorRef<Self>,
    ) -> Result<ConnectionId, SubscribeError> {
        let conn_id = self.alloc_conn_id();

        // 创建 ws_data_sink (PublicWsActor → ExchangeActor)
        let ws_data_sink = Arc::new(ExchangeDataSink::<C, S> {
            actor_ref: actor_ref.downgrade(),
        });

        // 创建 PublicWsActor 参数
        let ws_args = PublicWsActorArgs {
            conn_id,
            url: C::PUBLIC_WS_URL.to_string(),
            exchange: C::EXCHANGE,
            data_sink: ws_data_sink,
        };

        // 使用 spawn_link 创建并链接 PublicWsActor
        let ws_actor = spawn_link(actor_ref, PublicWsActor::new(ws_args)).await;

        // 记录 ActorID -> ConnectionId 映射
        self.actor_to_conn.insert(ws_actor.id(), conn_id);

        // 记录连接信息
        self.public_actors.insert(conn_id, ws_actor);
        self.conn_info.insert(
            conn_id,
            ConnectionInfo {
                conn_type: ConnectionType::Public,
                subscriptions: HashSet::new(),
            },
        );

        tracing::info!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            "PublicWsActor created with spawn_link"
        );

        Ok(conn_id)
    }

    /// 使用 ExchangeConfig::create_private_connection 创建私有连接
    async fn create_private_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
    ) -> Result<ConnectionId, SubscribeError> {
        let conn_id = self.alloc_conn_id();

        // 创建 ws_data_sink (PrivateWsActor → ExchangeActor)
        let ws_data_sink = Arc::new(ExchangeDataSink::<C, S> {
            actor_ref: actor_ref.downgrade(),
        });

        // 调用交易所特定的创建方法
        let handle = C::create_private_connection(
            &self.credentials,
            self.rest_client.clone(),
            ws_data_sink,
            conn_id,
            actor_ref.clone(),
        )
        .await
        .map_err(|e| SubscribeError::ConnectionFailed(e.to_string()))?;

        // 记录 ActorID -> ConnectionId 映射
        self.actor_to_conn.insert(handle.actor_id(), conn_id);

        // 记录连接信息
        self.conn_info.insert(
            conn_id,
            ConnectionInfo {
                conn_type: ConnectionType::Private,
                subscriptions: HashSet::new(),
            },
        );

        self.private_handle = Some(handle);

        tracing::info!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            "Private connection created with spawn_link"
        );

        Ok(conn_id)
    }

    /// 重启 PublicWsActor (在 on_link_died 中调用)
    async fn restart_public_actor(
        &mut self,
        actor_ref: &ActorRef<Self>,
        conn_id: ConnectionId,
    ) -> Result<(), SubscribeError> {
        // 获取连接信息
        let info = self.conn_info.get(&conn_id).cloned().ok_or_else(|| {
            SubscribeError::ConnectionFailed(format!("Connection {} not found", conn_id.0))
        })?;

        // 创建 ws_data_sink (PublicWsActor → ExchangeActor)
        let ws_data_sink = Arc::new(ExchangeDataSink::<C, S> {
            actor_ref: actor_ref.downgrade(),
        });

        // 创建 PublicWsActor 参数
        let ws_args = PublicWsActorArgs {
            conn_id,
            url: C::PUBLIC_WS_URL.to_string(),
            exchange: C::EXCHANGE,
            data_sink: ws_data_sink,
        };

        // 使用 spawn_link 创建新的 PublicWsActor
        let ws_actor = spawn_link(actor_ref, PublicWsActor::new(ws_args)).await;

        // 更新 ActorID -> ConnectionId 映射
        self.actor_to_conn.insert(ws_actor.id(), conn_id);

        // 更新 public_actors
        self.public_actors.insert(conn_id, ws_actor.clone());

        // 重新发送订阅
        for kind in &info.subscriptions {
            let msg = C::build_subscribe_msg(&[kind.clone()]);
            if !msg.is_empty() {
                let _ = ws_actor.tell(SendMessage(msg)).await;
            }
        }

        tracing::info!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            subscriptions = info.subscriptions.len(),
            "PublicWsActor restarted"
        );

        Ok(())
    }

    /// 重启 Private 连接 (在 on_link_died 中调用)
    async fn restart_private_connection(
        &mut self,
        actor_ref: &ActorRef<Self>,
        conn_id: ConnectionId,
    ) -> Result<(), SubscribeError> {
        // 获取连接信息
        let info = self.conn_info.get(&conn_id).cloned().ok_or_else(|| {
            SubscribeError::ConnectionFailed(format!("Connection {} not found", conn_id.0))
        })?;

        // 创建 ws_data_sink
        let ws_data_sink = Arc::new(ExchangeDataSink::<C, S> {
            actor_ref: actor_ref.downgrade(),
        });

        // 调用交易所特定的创建方法
        let handle = C::create_private_connection(
            &self.credentials,
            self.rest_client.clone(),
            ws_data_sink,
            conn_id,
            actor_ref.clone(),
        )
        .await
        .map_err(|e| SubscribeError::ConnectionFailed(e.to_string()))?;

        // 更新 ActorID -> ConnectionId 映射
        self.actor_to_conn.insert(handle.actor_id(), conn_id);

        // 重新发送订阅
        for kind in &info.subscriptions {
            let msg = C::build_subscribe_msg(&[kind.clone()]);
            if !msg.is_empty() {
                handle.send_message(msg).await;
            }
        }

        self.private_handle = Some(handle);

        tracing::info!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            subscriptions = info.subscriptions.len(),
            "Private connection restarted"
        );

        Ok(())
    }

    /// 发送订阅消息到 WebSocket
    async fn send_subscribe(&self, conn_id: ConnectionId, kind: &SubscriptionKind) {
        let msg = C::build_subscribe_msg(&[kind.clone()]);
        if msg.is_empty() {
            return;
        }

        // 检查连接类型
        if let Some(info) = self.conn_info.get(&conn_id) {
            match info.conn_type {
                ConnectionType::Public => {
                    if let Some(actor) = self.public_actors.get(&conn_id) {
                        let _ = actor.tell(SendMessage(msg)).await;
                    }
                }
                ConnectionType::Private => {
                    if let Some(handle) = &self.private_handle {
                        handle.send_message(msg).await;
                    }
                }
            }
        }
    }

    /// 发送取消订阅消息到 WebSocket
    async fn send_unsubscribe(&self, conn_id: ConnectionId, kind: &SubscriptionKind) {
        let msg = C::build_unsubscribe_msg(&[kind.clone()]);
        if msg.is_empty() {
            return;
        }

        // 检查连接类型
        if let Some(info) = self.conn_info.get(&conn_id) {
            match info.conn_type {
                ConnectionType::Public => {
                    if let Some(actor) = self.public_actors.get(&conn_id) {
                        let _ = actor.tell(SendMessage(msg)).await;
                    }
                }
                ConnectionType::Private => {
                    if let Some(handle) = &self.private_handle {
                        handle.send_message(msg).await;
                    }
                }
            }
        }
    }

    /// 检查并清理空闲连接
    fn cleanup_empty_connection(&mut self, conn_id: ConnectionId) {
        if let Some(info) = self.conn_info.get(&conn_id) {
            if info.subscriptions.is_empty() {
                match info.conn_type {
                    ConnectionType::Public => {
                        if let Some(ws_actor) = self.public_actors.remove(&conn_id) {
                            self.actor_to_conn.remove(&ws_actor.id());
                        }
                    }
                    ConnectionType::Private => {
                        if let Some(handle) = self.private_handle.take() {
                            self.actor_to_conn.remove(&handle.actor_id());
                        }
                    }
                }
                self.conn_info.remove(&conn_id);
                self.reconnect_backoff.remove(&conn_id);
            }
        }
    }

    /// 处理 WebSocket 数据
    async fn handle_ws_data(&mut self, data: WsData) {
        // 重置退避
        self.reset_backoff(data.conn_id);

        // 解析消息
        if let Some(parsed) = C::parse_message(&data.data) {
            self.handle_parsed_message(parsed).await;
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

        self.data_sink.send_market_data(market_data).await;
    }

    /// 判断错误是否可恢复
    ///
    /// 简化逻辑：Panicked 默认视为可恢复（网络错误等）
    /// 如果需要更精细的控制，可以在 WebSocketActor 中使用不同的停止方式
    fn is_recoverable_reason(reason: &ActorStopReason) -> bool {
        match reason {
            ActorStopReason::Normal => false, // 正常停止不需要重启
            ActorStopReason::Killed => false, // 被主动杀死不重启
            ActorStopReason::Panicked(_) => true, // 默认视为可恢复
            ActorStopReason::LinkDied { .. } => false, // 级联死亡不重启
        }
    }
}

impl<C: ExchangeWsProtocol, S: MarketDataSink> Actor for ExchangeActor<C, S> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ExchangeActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        self.self_ref = Some(actor_ref.clone());
        tracing::info!(exchange = %C::EXCHANGE, "ExchangeActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // 链接的子 Actor 会自动收到通知并停止
        tracing::info!(
            exchange = %C::EXCHANGE,
            reason = ?reason,
            "ExchangeActor stopped"
        );
        Ok(())
    }

    /// 处理链接的 Actor 死亡
    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorID,
        reason: ActorStopReason,
    ) -> Result<Option<ActorStopReason>, BoxError> {
        // 查找死掉的 WebSocketActor 对应的 ConnectionId
        let conn_id = match self.actor_to_conn.remove(&id) {
            Some(id) => id,
            None => {
                tracing::warn!(
                    exchange = %C::EXCHANGE,
                    actor_id = ?id,
                    "Unknown linked actor died"
                );
                return Ok(None);
            }
        };

        // 从对应的 map 中移除
        let conn_type = self.conn_info.get(&conn_id).map(|i| i.conn_type);
        match conn_type {
            Some(ConnectionType::Public) => {
                self.public_actors.remove(&conn_id);
            }
            Some(ConnectionType::Private) => {
                self.private_handle = None;
            }
            None => {}
        }

        tracing::warn!(
            exchange = %C::EXCHANGE,
            conn_id = conn_id.0,
            conn_type = ?conn_type,
            reason = ?reason,
            "WebSocket actor died"
        );

        // 判断是否可恢复
        if Self::is_recoverable_reason(&reason) {
            // 计算退避延迟
            let backoff = self.get_and_increase_backoff(conn_id);

            tracing::info!(
                exchange = %C::EXCHANGE,
                conn_id = conn_id.0,
                backoff_ms = backoff.as_millis(),
                "Will restart WebSocket actor after backoff"
            );

            // 等待退避时间
            tokio::time::sleep(backoff).await;

            // 根据连接类型重启
            if let Some(ar) = actor_ref.upgrade() {
                let result = match conn_type {
                    Some(ConnectionType::Public) => {
                        self.restart_public_actor(&ar, conn_id).await
                    }
                    Some(ConnectionType::Private) => {
                        self.restart_private_connection(&ar, conn_id).await
                    }
                    None => Err(SubscribeError::ConnectionFailed(
                        "Connection type unknown".to_string(),
                    )),
                };

                if let Err(e) = result {
                    tracing::error!(
                        exchange = %C::EXCHANGE,
                        conn_id = conn_id.0,
                        error = %e,
                        "Failed to restart WebSocket actor"
                    );
                    // 重启失败，继续尝试...可以选择级联 die
                }
            }

            Ok(None) // 继续运行
        } else {
            // 不可恢复，清理连接信息
            if let Some(info) = self.conn_info.remove(&conn_id) {
                // 清除订阅映射
                for kind in info.subscriptions {
                    self.subscription_to_conn.remove(&kind);
                }
            }
            self.reconnect_backoff.remove(&conn_id);

            // 对于 Normal 停止，继续运行
            if matches!(reason, ActorStopReason::Normal) {
                Ok(None)
            } else {
                // 其他不可恢复错误，级联 die
                tracing::error!(
                    exchange = %C::EXCHANGE,
                    conn_id = conn_id.0,
                    reason = ?reason,
                    "Unrecoverable error, propagating to parent"
                );
                Ok(Some(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }))
            }
        }
    }
}

// === Message Handlers ===

/// 内部消息: WebSocket 数据
struct InternalWsData(WsData);

impl<C: ExchangeWsProtocol, S: MarketDataSink> Message<InternalWsData> for ExchangeActor<C, S> {
    type Reply = ();

    async fn handle(&mut self, msg: InternalWsData, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_ws_data(msg.0).await;
    }
}

impl<C: ExchangeWsProtocol, S: MarketDataSink> Message<Subscribe> for ExchangeActor<C, S> {
    type Reply = ();

    async fn handle(&mut self, msg: Subscribe, _ctx: Context<'_, Self, Self::Reply>) {
        // 检查是否已订阅
        if self.subscription_to_conn.contains_key(&msg.kind) {
            return;
        }

        let actor_ref = match &self.self_ref {
            Some(r) => r.clone(),
            None => return,
        };

        // 惰性创建连接
        let conn_id = match &msg.kind {
            SubscriptionKind::FundingRate { .. } | SubscriptionKind::BBO { .. } => {
                match self.ensure_public_connection(&actor_ref).await {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!(
                            exchange = %C::EXCHANGE,
                            kind = ?msg.kind,
                            error = %e,
                            "Failed to create public connection"
                        );
                        return;
                    }
                }
            }
            SubscriptionKind::Private => match self.ensure_private_connection(&actor_ref).await {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(
                        exchange = %C::EXCHANGE,
                        error = %e,
                        "Failed to create private connection"
                    );
                    return;
                }
            },
        };

        // 发送订阅消息
        self.send_subscribe(conn_id, &msg.kind).await;

        // 记录订阅
        if let Some(info) = self.conn_info.get_mut(&conn_id) {
            info.subscriptions.insert(msg.kind.clone());
        }
        self.subscription_to_conn.insert(msg.kind, conn_id);
    }
}

impl<C: ExchangeWsProtocol, S: MarketDataSink> Message<Unsubscribe> for ExchangeActor<C, S> {
    type Reply = ();

    async fn handle(&mut self, msg: Unsubscribe, _ctx: Context<'_, Self, Self::Reply>) {
        if let Some(conn_id) = self.subscription_to_conn.remove(&msg.kind) {
            // 发送取消订阅消息
            self.send_unsubscribe(conn_id, &msg.kind).await;

            // 从连接的订阅集合中移除
            if let Some(info) = self.conn_info.get_mut(&conn_id) {
                info.subscriptions.remove(&msg.kind);
            }

            // 检查是否需要清理连接
            self.cleanup_empty_connection(conn_id);
        }
    }
}

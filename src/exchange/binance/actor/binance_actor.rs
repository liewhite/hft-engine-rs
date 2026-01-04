//! BinanceActor - Binance 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 接收子 actor 的 WsData 并解析
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - 将解析后的事件发送到 EventSink
//!
//! 架构:
//! BinanceActor (0 spawn)
//! ├── BinancePublicWsActor [spawn_link] (懒创建)
//! └── BinancePrivateWsActor [spawn_link] (懒创建, optional)
//!     └── BinanceListenKeyActor [spawn_link]

use super::private_ws::{BinancePrivateWsActor, BinancePrivateWsActorArgs};
use super::public_ws::{BinancePublicWsActor, BinancePublicWsActorArgs};
use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::{EventSink, SetEventSink, SetSymbolMetas, Subscribe, Unsubscribe, WsError};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// 子 Actor 类型
#[derive(Debug, Clone, Copy)]
enum ChildKind {
    PublicWs,
    PrivateWs,
}

/// BinanceActor 初始化参数
pub struct BinanceActorArgs {
    /// 凭证（可选）
    pub credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// REST 基础 URL（用于 ListenKey）
    pub rest_base_url: String,
}

/// BinanceActor - 父 Actor
pub struct BinanceActor {
    /// 凭证
    credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器（延迟注入）
    event_sink: Option<Arc<dyn EventSink>>,
    /// REST 基础 URL
    rest_base_url: String,

    /// 自身引用（用于懒创建子 Actor）
    self_ref: Option<ActorRef<Self>>,

    // 子 Actors (懒创建)
    /// Public WebSocket Actor
    public_ws: Option<ActorRef<BinancePublicWsActor>>,
    /// Private WebSocket Actor
    private_ws: Option<ActorRef<BinancePrivateWsActor>>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorID, ChildKind>,
}

impl BinanceActor {
    pub fn new(args: BinanceActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: None,
            rest_base_url: args.rest_base_url,
            self_ref: None,
            public_ws: None,
            private_ws: None,
            child_actors: HashMap::new(),
        }
    }

    /// 确保 PublicWsActor 存在（懒创建）
    async fn ensure_public_ws(&mut self) {
        if self.public_ws.is_some() {
            return;
        }

        let actor_ref = self
            .self_ref
            .as_ref()
            .expect("self_ref must be set in on_start");

        let public_ws = spawn_link(
            actor_ref,
            BinancePublicWsActor::new(BinancePublicWsActorArgs {
                parent: actor_ref.downgrade(),
            }),
        )
        .await;
        self.child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        self.public_ws = Some(public_ws);

        tracing::info!(exchange = "Binance", "PublicWsActor created (lazy)");
    }

    /// 确保 PrivateWsActor 存在（懒创建，需要凭证）
    async fn ensure_private_ws(&mut self) {
        if self.private_ws.is_some() || self.credentials.is_none() {
            return;
        }

        let actor_ref = self
            .self_ref
            .as_ref()
            .expect("self_ref must be set in on_start");
        let credentials = self.credentials.as_ref().unwrap();

        let private_ws = spawn_link(
            actor_ref,
            BinancePrivateWsActor::new(BinancePrivateWsActorArgs {
                parent: actor_ref.downgrade(),
                credentials: credentials.clone(),
                rest_base_url: self.rest_base_url.clone(),
            }),
        )
        .await;
        self.child_actors
            .insert(private_ws.id(), ChildKind::PrivateWs);
        self.private_ws = Some(private_ws);

        tracing::info!(exchange = "Binance", "PrivateWsActor created (lazy)");
    }
}

impl Actor for BinanceActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 只保存引用，不创建 WebSocket（等待 Subscribe 时懒创建）
        self.self_ref = Some(actor_ref);

        tracing::info!(
            exchange = "Binance",
            has_credentials = self.credentials.is_some(),
            "BinanceActor started (WebSocket will be created on first subscribe)"
        );

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("BinanceActor stopped");
        Ok(())
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorID,
        reason: ActorStopReason,
    ) -> Result<Option<ActorStopReason>, BoxError> {
        let kind = self.child_actors.remove(&id);

        match kind {
            Some(ChildKind::PublicWs) => {
                tracing::error!(reason = ?reason, "BinancePublicWsActor died, shutting down");
                self.public_ws = None;
            }
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "BinancePrivateWsActor died, shutting down");
                self.private_ws = None;
            }
            None => {
                tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
                return Ok(None);
            }
        }

        // 任何子 actor 死亡都级联退出
        Ok(Some(ActorStopReason::LinkDied {
            id,
            reason: Box::new(reason),
        }))
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<SetEventSink> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetEventSink,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.event_sink = Some(msg.event_sink);
        tracing::debug!(exchange = "Binance", "EventSink set");
    }
}

impl Message<SetSymbolMetas> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetSymbolMetas,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.symbol_metas = msg.symbol_metas;
        tracing::debug!(exchange = "Binance", "SymbolMetas set");
    }
}

impl Message<Subscribe> for BinanceActor {
    type Reply = ();

    async fn handle(&mut self, msg: Subscribe, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        // 懒创建 WebSocket actors
        self.ensure_public_ws().await;
        self.ensure_private_ws().await;

        // 转发给 PublicWsActor
        if let Some(ref public_ws) = self.public_ws {
            public_ws
                .tell(msg)
                .await
                .expect("Failed to forward Subscribe to PublicWsActor");
        }
    }
}

impl Message<Unsubscribe> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 转发给 PublicWsActor
        if let Some(ref public_ws) = self.public_ws {
            public_ws
                .tell(msg)
                .await
                .expect("Failed to forward Unsubscribe to PublicWsActor");
        }
    }
}

/// 内部 WebSocket 数据消息 (从子 actor 收到)
pub struct WsData {
    pub data: String,
}

impl Message<WsData> for BinanceActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        let event_sink = match &self.event_sink {
            Some(sink) => sink,
            None => {
                tracing::warn!("Received WsData but EventSink not set, dropping");
                return;
            }
        };

        let timestamp = now_ms();
        match parse_message(&msg.data, timestamp, &self.symbol_metas) {
            Ok(events) => {
                for event in events {
                    event_sink.send_event(event).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %msg.data, "Failed to parse Binance message");
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

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

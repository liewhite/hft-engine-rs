//! OkxActor - OKX 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 接收子 actor 的 WsData 并解析
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - 将解析后的事件发送到 EventSink
//!
//! 架构:
//! OkxActor (0 spawn)
//! ├── OkxPublicWsActor [spawn_link] (懒创建)
//! └── OkxPrivateWsActor [spawn_link] (懒创建, optional)

use super::private_ws::{OkxPrivateWsActor, OkxPrivateWsActorArgs};
use super::public_ws::{OkxPublicWsActor, OkxPublicWsActorArgs};
use crate::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, SetEventSink, SetSymbolMetas, Subscribe, Unsubscribe, WsError};
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsPush,
};
use crate::exchange::okx::OkxCredentials;
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

/// OkxActor 初始化参数
pub struct OkxActorArgs {
    /// 凭证（可选）
    pub credentials: Option<OkxCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// OkxActor - 父 Actor
pub struct OkxActor {
    /// 凭证
    credentials: Option<OkxCredentials>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器（延迟注入）
    event_sink: Option<Arc<dyn EventSink>>,

    /// 自身引用（用于懒创建子 Actor）
    self_ref: Option<ActorRef<Self>>,

    // 子 Actors (懒创建)
    /// Public WebSocket Actor
    public_ws: Option<ActorRef<OkxPublicWsActor>>,
    /// Private WebSocket Actor
    private_ws: Option<ActorRef<OkxPrivateWsActor>>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorID, ChildKind>,
}

impl OkxActor {
    pub fn new(args: OkxActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: None,
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
            OkxPublicWsActor::new(OkxPublicWsActorArgs {
                parent: actor_ref.downgrade(),
            }),
        )
        .await;
        self.child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        self.public_ws = Some(public_ws);

        tracing::info!(exchange = "OKX", "PublicWsActor created (lazy)");
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
            OkxPrivateWsActor::new(OkxPrivateWsActorArgs {
                parent: actor_ref.downgrade(),
                credentials: credentials.clone(),
            }),
        )
        .await;
        self.child_actors
            .insert(private_ws.id(), ChildKind::PrivateWs);
        self.private_ws = Some(private_ws);

        tracing::info!(exchange = "OKX", "PrivateWsActor created (lazy)");
    }
}

impl Actor for OkxActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 只保存引用，不创建 WebSocket（等待 Subscribe 时懒创建）
        self.self_ref = Some(actor_ref);

        tracing::info!(
            exchange = "OKX",
            has_credentials = self.credentials.is_some(),
            "OkxActor started (WebSocket will be created on first subscribe)"
        );

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("OkxActor stopped");
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
                tracing::error!(reason = ?reason, "OkxPublicWsActor died, shutting down");
                self.public_ws = None;
            }
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "OkxPrivateWsActor died, shutting down");
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

impl Message<SetEventSink> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetEventSink,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.event_sink = Some(msg.event_sink);
        tracing::debug!(exchange = "OKX", "EventSink set");
    }
}

impl Message<SetSymbolMetas> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetSymbolMetas,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.symbol_metas = msg.symbol_metas;
        tracing::debug!(exchange = "OKX", "SymbolMetas set");
    }
}

impl Message<Subscribe> for OkxActor {
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

impl Message<Unsubscribe> for OkxActor {
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

impl Message<WsData> for OkxActor {
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
                tracing::error!(error = %e, raw = %msg.data, "Failed to parse OKX message");
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
                    let meta = symbol_metas
                        .get(&position.symbol)
                        .expect("SymbolMeta not found for position symbol");
                    position.size = meta.qty_to_coin(position.size);
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
            tracing::warn!(channel, raw, "Unknown OKX channel");
            Ok(Vec::new())
        }
    }
}

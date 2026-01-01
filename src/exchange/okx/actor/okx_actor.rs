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
//! ├── OkxPublicWsActor [spawn_link]
//! └── OkxPrivateWsActor [spawn_link] (optional)

use super::private_ws::{OkxPrivateWsActor, OkxPrivateWsActorArgs};
use super::public_ws::{OkxPublicWsActor, OkxPublicWsActorArgs};
use crate::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe, WsError};
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
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
}

/// OkxActor - 父 Actor
pub struct OkxActor {
    /// 凭证
    credentials: Option<OkxCredentials>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,

    // 子 Actors
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
            event_sink: args.event_sink,
            public_ws: None,
            private_ws: None,
            child_actors: HashMap::new(),
        }
    }
}

impl Actor for OkxActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        let weak_ref = actor_ref.downgrade();

        // 1. spawn_link PublicWsActor
        let public_ws = spawn_link(
            &actor_ref,
            OkxPublicWsActor::new(OkxPublicWsActorArgs {
                parent: weak_ref.clone(),
            }),
        )
        .await;
        self.child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        self.public_ws = Some(public_ws);

        // 2. 如果有凭证，spawn_link PrivateWsActor
        if let Some(credentials) = &self.credentials {
            let private_ws = spawn_link(
                &actor_ref,
                OkxPrivateWsActor::new(OkxPrivateWsActorArgs {
                    parent: weak_ref,
                    credentials: credentials.clone(),
                }),
            )
            .await;
            self.child_actors
                .insert(private_ws.id(), ChildKind::PrivateWs);
            self.private_ws = Some(private_ws);
        }

        tracing::info!(
            exchange = "OKX",
            has_private = self.private_ws.is_some(),
            "OkxActor started"
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

impl Message<Subscribe> for OkxActor {
    type Reply = ();

    async fn handle(&mut self, msg: Subscribe, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
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

//! HyperliquidActor - Hyperliquid 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 子 actor
//! - 接收子 actor 的 WsData 并解析
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - 将解析后的事件发送到 EventSink
//!
//! 架构:
//! HyperliquidActor
//! └── HyperliquidPublicWsActor [spawn_link]

use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use super::WsData;
use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe, WsError};
use crate::exchange::hyperliquid::codec::{WsActiveAssetCtx, WsBbo};
use crate::exchange::hyperliquid::HyperliquidCredentials;
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
}

/// HyperliquidActor 初始化参数
pub struct HyperliquidActorArgs {
    /// 凭证（可选）
    pub credentials: Option<HyperliquidCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
}

/// HyperliquidActor - 父 Actor
pub struct HyperliquidActor {
    /// 凭证
    #[allow(dead_code)]
    credentials: Option<HyperliquidCredentials>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,

    // 子 Actors
    /// Public WebSocket Actor
    public_ws: Option<ActorRef<HyperliquidPublicWsActor>>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorID, ChildKind>,
}

impl HyperliquidActor {
    pub fn new(args: HyperliquidActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: args.event_sink,
            public_ws: None,
            child_actors: HashMap::new(),
        }
    }
}

impl Actor for HyperliquidActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "HyperliquidActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        let weak_ref = actor_ref.downgrade();

        // spawn_link PublicWsActor
        let public_ws = spawn_link(
            &actor_ref,
            HyperliquidPublicWsActor::new(HyperliquidPublicWsActorArgs {
                parent: weak_ref,
            }),
        )
        .await;
        self.child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        self.public_ws = Some(public_ws);

        tracing::info!(exchange = "Hyperliquid", "HyperliquidActor started");

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("HyperliquidActor stopped");
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
                tracing::error!(reason = ?reason, "HyperliquidPublicWsActor died, shutting down");
                self.public_ws = None;
            }
            None => {
                tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
                return Ok(None);
            }
        }

        // 子 actor 死亡级联退出
        Ok(Some(ActorStopReason::LinkDied {
            id,
            reason: Box::new(reason),
        }))
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for HyperliquidActor {
    type Reply = ();

    async fn handle(&mut self, msg: Subscribe, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        // 转发给 PublicWsActor
        if let Some(ref public_ws) = self.public_ws {
            public_ws
                .tell(msg)
                .await
                .expect("Failed to forward Subscribe to PublicWsActor");
        }
    }
}

impl Message<Unsubscribe> for HyperliquidActor {
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

impl Message<WsData> for HyperliquidActor {
    type Reply = ();

    async fn handle(&mut self, msg: WsData, _ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        let local_ts = now_ms();
        match parse_message(&msg.data, local_ts) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %msg.data, "Failed to parse Hyperliquid message");
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_message(raw: &str, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是订阅确认
    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            "subscriptionResponse" => {
                // 订阅响应，忽略
                return Ok(Vec::new());
            }
            "activeAssetCtx" => {
                // 资产上下文（包含资金费率）
                let data = &value["data"];
                if let Ok(ctx) = serde_json::from_value::<WsActiveAssetCtx>(data.clone()) {
                    let mut events = Vec::new();

                    // 资金费率事件
                    let rate = ctx.to_funding_rate();
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::FundingRate(rate),
                    });

                    // 如果有 impact_pxs，也生成 BBO 事件
                    if let Some(bbo) = ctx.to_bbo() {
                        events.push(IncomeEvent {
                            exchange_ts: local_ts,
                            local_ts,
                            data: ExchangeEventData::BBO(bbo),
                        });
                    }

                    return Ok(events);
                }
            }
            "bbo" => {
                // BBO 数据
                let data = &value["data"];
                if let Ok(bbo_data) = serde_json::from_value::<WsBbo>(data.clone()) {
                    let bbo = bbo_data.to_bbo();
                    return Ok(vec![IncomeEvent {
                        exchange_ts: bbo.timestamp,
                        local_ts,
                        data: ExchangeEventData::BBO(bbo),
                    }]);
                }
            }
            "allMids" => {
                // 所有中间价，当前不处理
                return Ok(Vec::new());
            }
            _ => {
                tracing::debug!(channel, "Unknown Hyperliquid channel");
                return Ok(Vec::new());
            }
        }
    }

    // pong 消息
    if value.get("method").map(|v| v.as_str()) == Some(Some("pong")) {
        return Ok(Vec::new());
    }

    // 其他未知消息
    tracing::debug!(raw, "Unhandled Hyperliquid message");
    Ok(Vec::new())
}

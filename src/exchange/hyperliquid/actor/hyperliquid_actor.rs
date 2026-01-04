//! HyperliquidActor - Hyperliquid 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 接收子 actor 的 WsData 并解析
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - 将解析后的事件发送到 EventSink
//!
//! 架构:
//! HyperliquidActor (0 spawn)
//! ├── HyperliquidPublicWsActor [spawn_link] (懒创建)
//! └── HyperliquidPrivateWsActor [spawn_link] (懒创建, optional)

use super::private_ws::{HyperliquidPrivateWsActor, HyperliquidPrivateWsActorArgs};
use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use super::WsData;
use crate::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe, WsError};
use crate::exchange::hyperliquid::codec::{
    ClearinghouseState, WsActiveAssetCtx, WsBbo, WsOrderUpdate,
};
use crate::exchange::hyperliquid::HyperliquidCredentials;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{spawn_link, ActorID, ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

/// 子 Actor 类型
#[derive(Debug, Clone, Copy)]
enum ChildKind {
    PublicWs,
    PrivateWs,
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
    /// 凭证（用于下单，包含钱包地址）
    credentials: Option<HyperliquidCredentials>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,

    /// 自身引用（用于懒创建子 Actor）
    self_ref: Option<ActorRef<Self>>,

    // 子 Actors (懒创建)
    /// Public WebSocket Actor
    public_ws: Option<ActorRef<HyperliquidPublicWsActor>>,
    /// Private WebSocket Actor (账户订阅)
    private_ws: Option<ActorRef<HyperliquidPrivateWsActor>>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorID, ChildKind>,
}

impl HyperliquidActor {
    pub fn new(args: HyperliquidActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            symbol_metas: args.symbol_metas,
            event_sink: args.event_sink,
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
            HyperliquidPublicWsActor::new(HyperliquidPublicWsActorArgs {
                parent: actor_ref.downgrade(),
            }),
        )
        .await;
        self.child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        self.public_ws = Some(public_ws);

        tracing::info!(exchange = "Hyperliquid", "PublicWsActor created (lazy)");
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
            HyperliquidPrivateWsActor::new(HyperliquidPrivateWsActorArgs {
                parent: actor_ref.downgrade(),
                wallet_address: credentials.wallet_address.clone(),
            }),
        )
        .await;
        self.child_actors
            .insert(private_ws.id(), ChildKind::PrivateWs);
        self.private_ws = Some(private_ws);

        tracing::info!(exchange = "Hyperliquid", "PrivateWsActor created (lazy)");
    }
}

impl Actor for HyperliquidActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "HyperliquidActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 只保存引用，不创建 WebSocket（等待 Subscribe 时懒创建）
        self.self_ref = Some(actor_ref);

        tracing::info!(
            exchange = "Hyperliquid",
            has_credentials = self.credentials.is_some(),
            "HyperliquidActor started (WebSocket will be created on first subscribe)"
        );

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
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "HyperliquidPrivateWsActor died, shutting down");
                self.private_ws = None;
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
                match serde_json::from_value::<WsActiveAssetCtx>(data.clone()) {
                    Ok(ctx) => {
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
                    Err(e) => {
                        tracing::warn!(error = %e, data = %data, "Failed to parse activeAssetCtx");
                    }
                }
            }
            "bbo" => {
                // BBO 数据
                let data = &value["data"];
                match serde_json::from_value::<WsBbo>(data.clone()) {
                    Ok(bbo_data) => {
                        let bbo = bbo_data.to_bbo();
                        return Ok(vec![IncomeEvent {
                            exchange_ts: bbo.timestamp,
                            local_ts,
                            data: ExchangeEventData::BBO(bbo),
                        }]);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, data = %data, "Failed to parse bbo");
                    }
                }
            }
            "allMids" => {
                // 所有中间价，当前不处理
                return Ok(Vec::new());
            }
            "webData3" => {
                // 账户状态 (positions, balance)
                let data = &value["data"];
                return parse_web_data3(data, local_ts);
            }
            "orderUpdates" => {
                // 订单更新
                let data = &value["data"];
                return parse_order_updates(data, local_ts);
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

/// 解析 webData3 消息 (账户状态)
fn parse_web_data3(data: &serde_json::Value, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // 解析 clearinghouseState
    if let Some(ch_state) = data.get("clearinghouseState") {
        match serde_json::from_value::<ClearinghouseState>(ch_state.clone()) {
            Ok(state) => {
                // 解析仓位
                for wrapper in &state.asset_positions {
                    let position = wrapper.position.to_position();
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(position),
                    });
                }

                // 解析账户净值 (equity = accountValue)
                let equity = f64::from_str(&state.cross_margin_summary.account_value)
                    .expect("accountValue must be valid float from Hyperliquid API");
                events.push(IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::Equity {
                        exchange: Exchange::Hyperliquid,
                        equity,
                    },
                });

                // 解析可用余额
                let withdrawable = f64::from_str(&state.withdrawable)
                    .expect("withdrawable must be valid float from Hyperliquid API");
                events.push(IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::Balance(crate::domain::Balance {
                        exchange: Exchange::Hyperliquid,
                        asset: "USDC".to_string(),
                        available: withdrawable,
                        frozen: 0.0, // Hyperliquid 不直接提供 frozen，通过 marginUsed 计算
                    }),
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, data = %ch_state, "Failed to parse clearinghouseState");
            }
        }
    }

    Ok(events)
}

/// 解析 orderUpdates 消息
fn parse_order_updates(
    data: &serde_json::Value,
    local_ts: u64,
) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // orderUpdates 是一个数组
    if let Some(updates) = data.as_array() {
        for update in updates {
            match serde_json::from_value::<WsOrderUpdate>(update.clone()) {
                Ok(order_update) => {
                    let update = order_update.to_order_update();
                    events.push(IncomeEvent {
                        exchange_ts: update.timestamp,
                        local_ts,
                        data: ExchangeEventData::OrderUpdate(update),
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, data = %update, "Failed to parse order update");
                }
            }
        }
    }

    Ok(events)
}

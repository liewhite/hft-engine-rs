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
//! ├── BinancePublicWsActor [spawn_link]
//! └── BinancePrivateWsActor [spawn_link] (optional)
//!     └── BinanceListenKeyActor [spawn_link]

use super::private_ws::{BinancePrivateWsActor, BinancePrivateWsActorArgs};
use super::public_ws::{BinancePublicWsActor, BinancePublicWsActorArgs};
use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe, WsError};
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
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// REST 基础 URL（用于 ListenKey）
    pub rest_base_url: String,
}

/// BinanceActor - 父 Actor
pub struct BinanceActor {
    /// 凭证
    credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
    /// REST 基础 URL
    rest_base_url: String,

    // 子 Actors
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
            event_sink: args.event_sink,
            rest_base_url: args.rest_base_url,
            public_ws: None,
            private_ws: None,
            child_actors: HashMap::new(),
        }
    }
}

impl Actor for BinanceActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        let weak_ref = actor_ref.downgrade();

        // 1. spawn_link PublicWsActor
        let public_ws = spawn_link(
            &actor_ref,
            BinancePublicWsActor::new(BinancePublicWsActorArgs {
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
                BinancePrivateWsActor::new(BinancePrivateWsActorArgs {
                    parent: weak_ref,
                    credentials: credentials.clone(),
                    rest_base_url: self.rest_base_url.clone(),
                }),
            )
            .await;
            self.child_actors
                .insert(private_ws.id(), ChildKind::PrivateWs);
            self.private_ws = Some(private_ws);
        }

        tracing::info!(
            exchange = "Binance",
            has_private = self.private_ws.is_some(),
            "BinanceActor started"
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

impl Message<Subscribe> for BinanceActor {
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

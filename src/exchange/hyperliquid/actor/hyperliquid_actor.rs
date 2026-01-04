//! HyperliquidActor - Hyperliquid 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发送到 EventSink
//!
//! 架构:
//! HyperliquidActor (0 spawn)
//! ├── HyperliquidPublicWsActor [spawn_link] (懒创建)
//! └── HyperliquidPrivateWsActor [spawn_link] (懒创建, optional)

use super::private_ws::{HyperliquidPrivateWsActor, HyperliquidPrivateWsActorArgs};
use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe};
use crate::exchange::hyperliquid::HyperliquidCredentials;
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
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,

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
            public_ws: None,
            private_ws: None,
            child_actors: HashMap::new(),
        }
    }

    /// 确保 PublicWsActor 存在（懒创建）
    async fn ensure_public_ws(&mut self, actor_ref: &ActorRef<Self>) {
        if self.public_ws.is_some() {
            return;
        }

        let public_ws = spawn_link(
            actor_ref,
            HyperliquidPublicWsActor::new(HyperliquidPublicWsActorArgs {
                event_sink: self.event_sink.clone(),
                symbol_metas: self.symbol_metas.clone(),
            }),
        )
        .await;
        self.child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        self.public_ws = Some(public_ws);

        tracing::info!(exchange = "Hyperliquid", "PublicWsActor created (lazy)");
    }

    /// 确保 PrivateWsActor 存在（懒创建，需要凭证）
    async fn ensure_private_ws(&mut self, actor_ref: &ActorRef<Self>) {
        if self.private_ws.is_some() || self.credentials.is_none() {
            return;
        }

        let credentials = self.credentials.as_ref().unwrap();

        let private_ws = spawn_link(
            actor_ref,
            HyperliquidPrivateWsActor::new(HyperliquidPrivateWsActorArgs {
                wallet_address: credentials.wallet_address.clone(),
                event_sink: self.event_sink.clone(),
                symbol_metas: self.symbol_metas.clone(),
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

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
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

    async fn handle(&mut self, msg: Subscribe, ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        let actor_ref = ctx.actor_ref();

        // 懒创建 WebSocket actors
        self.ensure_public_ws(&actor_ref).await;
        self.ensure_private_ws(&actor_ref).await;

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

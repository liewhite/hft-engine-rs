//! HyperliquidActor - Hyperliquid 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发送到 EventSink
//!
//! 架构:
//! HyperliquidActor (父)
//! ├── HyperliquidPublicWsActor [spawn_link]
//! └── HyperliquidPrivateWsActor [spawn_link] (optional, 需要凭证)

use super::private_ws::{HyperliquidPrivateWsActor, HyperliquidPrivateWsActorArgs};
use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe};
use crate::exchange::hyperliquid::HyperliquidCredentials;
use kameo::actor::{spawn_link, ActorID, ActorRef, PreparedActor, WeakActorRef};
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
    /// Public WebSocket Actor
    public_ws: ActorRef<HyperliquidPublicWsActor>,
    /// Private WebSocket Actor (可选，需要凭证)
    _private_ws: Option<ActorRef<HyperliquidPrivateWsActor>>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorID, ChildKind>,
}

impl HyperliquidActor {
    /// 创建 HyperliquidActor 并返回 ActorRef
    ///
    /// 使用 PreparedActor 模式，在构造时 spawn_link 所有子 actors
    pub async fn new(args: HyperliquidActorArgs) -> ActorRef<Self> {
        // 1. 准备 actor，获取 actor_ref
        let prepared = PreparedActor::<Self>::new();
        let actor_ref = prepared.actor_ref().clone();

        let mut child_actors = HashMap::new();

        // 3. 创建 PublicWsActor
        let public_ws = spawn_link(
            &actor_ref,
            HyperliquidPublicWsActor::new(HyperliquidPublicWsActorArgs {
                event_sink: args.event_sink.clone(),
                symbol_metas: args.symbol_metas.clone(),
            }),
        )
        .await;
        child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        tracing::info!(exchange = "Hyperliquid", "PublicWsActor created");

        // 4. 创建 PrivateWsActor (如果有凭证)
        let private_ws = if let Some(credentials) = args.credentials {
            let private_ws = spawn_link(
                &actor_ref,
                HyperliquidPrivateWsActor::new(HyperliquidPrivateWsActorArgs {
                    wallet_address: credentials.wallet_address,
                    event_sink: args.event_sink,
                    symbol_metas: args.symbol_metas,
                }),
            )
            .await;
            child_actors.insert(private_ws.id(), ChildKind::PrivateWs);
            tracing::info!(exchange = "Hyperliquid", "PrivateWsActor created");
            Some(private_ws)
        } else {
            None
        };

        // 5. 构造 actor 并 spawn
        let actor = Self {
            public_ws,
            _private_ws: private_ws,
            child_actors,
        };

        prepared.spawn(actor);

        actor_ref
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
            has_private_ws = self._private_ws.is_some(),
            "HyperliquidActor started"
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
            }
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "HyperliquidPrivateWsActor died, shutting down");
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
        self.public_ws
            .tell(msg)
            .await
            .expect("Failed to forward Subscribe to PublicWsActor");
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
        self.public_ws
            .tell(msg)
            .await
            .expect("Failed to forward Unsubscribe to PublicWsActor");
    }
}

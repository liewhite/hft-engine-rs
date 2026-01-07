//! OkxActor - OKX 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到 IncomePubSub
//!
//! 架构:
//! OkxActor (父)
//! ├── OkxPublicWsActor [spawn + link]
//! └── OkxPrivateWsActor [spawn + link] (optional, 需要凭证)

use super::private_ws::{OkxPrivateWsActor, OkxPrivateWsActorArgs};
use super::public_ws::{OkxPublicWsActor, OkxPublicWsActorArgs};
use crate::domain::{Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, Unsubscribe};
use crate::exchange::okx::OkxCredentials;
use kameo::actor::{ActorId, ActorRef, PreparedActor, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::ops::ControlFlow;
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
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
}

/// OkxActor - 父 Actor
pub struct OkxActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<OkxPublicWsActor>,
    /// Private WebSocket Actor (可选，需要凭证)
    _private_ws: Option<ActorRef<OkxPrivateWsActor>>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorId, ChildKind>,
}

impl OkxActor {
    /// 创建 OkxActor 并返回 ActorRef
    ///
    /// 使用 PreparedActor 模式，在构造时 spawn + link 所有子 actors
    pub async fn new(args: OkxActorArgs) -> ActorRef<Self> {
        // 1. 准备 actor，获取 actor_ref
        let prepared = PreparedActor::<Self>::new(mailbox::unbounded());
        let actor_ref = prepared.actor_ref().clone();

        let mut child_actors = HashMap::new();

        // 2. 创建 PublicWsActor
        let public_ws = OkxPublicWsActor::spawn_with_mailbox(
            OkxPublicWsActorArgs {
                income_pubsub: args.income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
            },
            mailbox::unbounded(),
        );
        actor_ref.link(&public_ws).await;
        child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        tracing::info!(exchange = "OKX", "PublicWsActor created");

        // 3. 创建 PrivateWsActor (如果有凭证)
        let private_ws = if let Some(credentials) = args.credentials {
            let private_ws = OkxPrivateWsActor::spawn_with_mailbox(
                OkxPrivateWsActorArgs {
                    credentials,
                    income_pubsub: args.income_pubsub,
                    symbol_metas: args.symbol_metas,
                },
                mailbox::unbounded(),
            );
            actor_ref.link(&private_ws).await;
            child_actors.insert(private_ws.id(), ChildKind::PrivateWs);
            tracing::info!(exchange = "OKX", "PrivateWsActor created");
            Some(private_ws)
        } else {
            None
        };

        // 4. 构造 actor 并 spawn
        let actor = Self {
            public_ws,
            _private_ws: private_ws,
            child_actors,
        };

        prepared.spawn(actor);

        actor_ref
    }
}

impl Actor for OkxActor {
    type Args = Self;
    type Error = Infallible;

    async fn on_start(state: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::info!(
            exchange = "OKX",
            has_private_ws = state._private_ws.is_some(),
            "OkxActor started"
        );

        Ok(state)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("OkxActor stopped");
        Ok(())
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        let kind = self.child_actors.remove(&id);

        match kind {
            Some(ChildKind::PublicWs) => {
                tracing::error!(reason = ?reason, "OkxPublicWsActor died, shutting down");
            }
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "OkxPrivateWsActor died, shutting down");
            }
            None => {
                tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
                return Ok(ControlFlow::Continue(()));
            }
        }

        // 任何子 actor 死亡都级联退出
        Ok(ControlFlow::Break(ActorStopReason::LinkDied {
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

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 转发给 PublicWsActor
        let _ = self.public_ws.tell(msg).send().await;
    }
}

impl Message<Unsubscribe> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 转发给 PublicWsActor
        let _ = self.public_ws.tell(msg).send().await;
    }
}

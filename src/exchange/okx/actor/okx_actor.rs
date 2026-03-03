//! OkxActor - OKX 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到 IncomePubSub
//!
//! 架构:
//! OkxActor (父)
//! ├── OkxPublicWsActor [spawn_link]
//! └── OkxPrivateWsActor [spawn_link] (optional, 需要凭证)

use super::private_ws::{OkxPrivateWsActor, OkxPrivateWsActorArgs};
use super::public_ws::{OkxPublicWsActor, OkxPublicWsActorArgs};
use crate::domain::{Symbol, SymbolMeta};
use crate::engine::{CryptoStatusActor, CryptoStatusActorArgs, IncomePubSub};
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::okx::OkxCredentials;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

/// OkxActor 初始化参数
pub struct OkxActorArgs {
    /// 凭证（可选）
    pub credentials: Option<OkxCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// OkxActor - 父 Actor
pub struct OkxActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<OkxPublicWsActor>,
}

impl Actor for OkxActor {
    type Args = OkxActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let income_pubsub = args.income_pubsub;

        // 1. 创建 PublicWsActor (使用 spawn_link_with_mailbox)
        let public_ws = OkxPublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            OkxPublicWsActorArgs {
                income_pubsub: income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "OKX", "PublicWsActor created");

        // 2. 创建 PrivateWsActor (如果有凭证)
        let has_private_ws = if let Some(credentials) = args.credentials {
            OkxPrivateWsActor::spawn_link_with_mailbox(
                &actor_ref,
                OkxPrivateWsActorArgs {
                    credentials,
                    income_pubsub: income_pubsub.clone(),
                    symbol_metas: args.symbol_metas,
                },
                mailbox::unbounded(),
            )
            .await;
            tracing::info!(exchange = "OKX", "PrivateWsActor created");
            true
        } else {
            false
        };

        // 3. 创建 CryptoStatusActor (加密货币 7x24 始终 Liquid)
        CryptoStatusActor::spawn_link_with_mailbox(
            &actor_ref,
            CryptoStatusActorArgs {
                exchange: crate::domain::Exchange::OKX,
                income_pubsub,
                interval_ms: 60_000,
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "OKX", "CryptoStatusActor created");

        tracing::info!(
            exchange = "OKX",
            has_private_ws,
            "OkxActor started"
        );

        Ok(Self { public_ws })
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
        tracing::error!(actor_id = ?id, reason = ?reason, "Child actor died, shutting down");
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

impl Message<SubscribeBatch> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
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

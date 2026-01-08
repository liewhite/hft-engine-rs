//! HyperliquidActor - Hyperliquid 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到 IncomePubSub
//!
//! 架构:
//! HyperliquidActor (父)
//! ├── HyperliquidPublicWsActor [spawn_link]
//! └── HyperliquidPrivateWsActor [spawn_link] (optional, 需要凭证)

use super::private_ws::{HyperliquidPrivateWsActor, HyperliquidPrivateWsActorArgs};
use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use crate::domain::{Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::hyperliquid::HyperliquidCredentials;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

/// HyperliquidActor 初始化参数
pub struct HyperliquidActorArgs {
    /// 凭证（可选）
    pub credentials: Option<HyperliquidCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 计价币种 (e.g., "USDC", "USDE")
    pub quote: String,
}

/// HyperliquidActor - 父 Actor
pub struct HyperliquidActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<HyperliquidPublicWsActor>,
}

impl Actor for HyperliquidActor {
    type Args = HyperliquidActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 创建 PublicWsActor (使用 spawn_link_with_mailbox)
        let public_ws = HyperliquidPublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            HyperliquidPublicWsActorArgs {
                income_pubsub: args.income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "Hyperliquid", "PublicWsActor created");

        // 2. 创建 PrivateWsActor (如果有凭证)
        let has_private_ws = if let Some(credentials) = args.credentials {
            HyperliquidPrivateWsActor::spawn_link_with_mailbox(
                &actor_ref,
                HyperliquidPrivateWsActorArgs {
                    wallet_address: credentials.wallet_address,
                    income_pubsub: args.income_pubsub,
                    symbol_metas: args.symbol_metas,
                },
                mailbox::unbounded(),
            )
            .await;
            tracing::info!(exchange = "Hyperliquid", "PrivateWsActor created");
            true
        } else {
            false
        };

        tracing::info!(
            exchange = "Hyperliquid",
            has_private_ws,
            "HyperliquidActor started"
        );

        Ok(Self { public_ws })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("HyperliquidActor stopped");
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

impl Message<Subscribe> for HyperliquidActor {
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

impl Message<SubscribeBatch> for HyperliquidActor {
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

impl Message<Unsubscribe> for HyperliquidActor {
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

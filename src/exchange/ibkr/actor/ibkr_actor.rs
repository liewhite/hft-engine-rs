//! IbkrActor - IBKR 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - 维护 tickle 保活任务
//!
//! 架构:
//! IbkrActor (父)
//! └── IbkrPublicWsActor [spawn_link]

use super::public_ws::{IbkrPublicWsActor, IbkrPublicWsActorArgs};
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::ibkr::oauth::IbkrOAuth;
use crate::engine::IncomePubSub;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use reqwest::Client;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use tokio::sync::RwLock;

/// IbkrActor 初始化参数
pub struct IbkrActorArgs {
    /// OAuth 认证器 (共享)
    pub oauth: Arc<RwLock<IbkrOAuth>>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// conid 映射 (symbol → conid)
    pub conids: HashMap<String, i64>,
}

/// IbkrActor - 父 Actor
pub struct IbkrActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<IbkrPublicWsActor>,
}

impl Actor for IbkrActor {
    type Args = IbkrActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 创建 PublicWsActor
        let public_ws = IbkrPublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            IbkrPublicWsActorArgs {
                oauth: args.oauth.clone(),
                income_pubsub: args.income_pubsub,
                conids: args.conids,
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "IBKR", "PublicWsActor created");

        // 2. 启动 tickle 保活任务 (每 60s POST /tickle)
        let oauth = args.oauth;
        tokio::spawn(async move {
            let http = Client::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let oauth_guard = oauth.read().await;
                let base_url = oauth_guard.base_url().to_string();
                let tickle_url = format!("{}tickle", base_url);
                match oauth_guard.sign_request("POST", &tickle_url, None) {
                    Ok(auth) => {
                        let _ = http
                            .post(&tickle_url)
                            .header("Authorization", &auth)
                            .header("User-Agent", "ibind-rs")
                            .send()
                            .await;
                        tracing::trace!("IBKR tickle sent");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "IBKR tickle sign failed");
                    }
                }
            }
        });

        tracing::info!(exchange = "IBKR", "IbkrActor started");

        Ok(Self { public_ws })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("IbkrActor stopped");
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

impl Message<Subscribe> for IbkrActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let _ = self.public_ws.tell(msg).send().await;
    }
}

impl Message<SubscribeBatch> for IbkrActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let _ = self.public_ws.tell(msg).send().await;
    }
}

impl Message<Unsubscribe> for IbkrActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let _ = self.public_ws.tell(msg).send().await;
    }
}

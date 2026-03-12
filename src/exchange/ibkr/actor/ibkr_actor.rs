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

use super::position_polling::{IbkrPositionPollingActor, IbkrPositionPollingActorArgs};
use super::public_ws::{IbkrPublicWsActor, IbkrPublicWsActorArgs};
use super::status_polling::{IbkrStatusPollingActor, IbkrStatusPollingActorArgs};
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::ibkr::auth::{self, IbkrAuth};
use crate::exchange::ibkr::IbkrClient;
use crate::engine::IncomePubSub;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

/// IBKR 持仓轮询间隔 (毫秒)
const POSITION_POLLING_INTERVAL_MS: u64 = 3000;

/// 市场状态轮询间隔 (毫秒)
const STATUS_POLLING_INTERVAL_MS: u64 = 60_000;

/// IbkrActor 初始化参数
pub struct IbkrActorArgs {
    /// 认证器 (共享，不可变)
    pub auth: Arc<dyn IbkrAuth>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// conid 映射 (symbol → conid)
    pub conids: HashMap<String, i64>,
    /// IBKR 客户端 (用于持仓轮询)
    pub client: Arc<IbkrClient>,
}

/// IbkrActor - 父 Actor
pub struct IbkrActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<IbkrPublicWsActor>,
    /// 持仓轮询 Actor
    _position_polling: ActorRef<IbkrPositionPollingActor>,
    /// 市场状态轮询 Actor
    _status_polling: ActorRef<IbkrStatusPollingActor>,
}

impl Actor for IbkrActor {
    type Args = IbkrActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. Tickle 获取 session_id (供 WS Cookie 使用)
        let http = args
            .auth
            .build_http_client()
            .expect("Failed to build HTTP client");
        let session_id = auth::tickle(&*args.auth, &http)
            .await
            .expect("Initial tickle failed");

        // 2. 创建 PublicWsActor (传入 session_id)
        let income_pubsub = args.income_pubsub;
        let public_ws = IbkrPublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            IbkrPublicWsActorArgs {
                auth: args.auth.clone(),
                income_pubsub: income_pubsub.clone(),
                conids: args.conids,
                session_id,
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "IBKR", "PublicWsActor created");

        // 3. 启动 tickle 保活任务 (每 60s POST /tickle)
        // 连续失败 3 次则 kill actor，触发级联退出
        let auth = args.auth;
        let weak_ref = actor_ref.downgrade();
        tokio::spawn(async move {
            const MAX_CONSECUTIVE_FAILURES: u32 = 3;

            let mut consecutive_failures: u32 = 0;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let actor = match weak_ref.upgrade() {
                    Some(a) => a,
                    None => {
                        tracing::info!("IBKR tickle task: actor stopped, exiting");
                        break;
                    }
                };

                let succeeded = auth::tickle(&*auth, &http).await.is_ok();

                if succeeded {
                    consecutive_failures = 0;
                    tracing::trace!("IBKR tickle sent");
                } else {
                    consecutive_failures += 1;
                    tracing::warn!(
                        consecutive_failures,
                        max = MAX_CONSECUTIVE_FAILURES,
                        "IBKR tickle failed"
                    );
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        tracing::error!("IBKR tickle: {} consecutive failures, killing actor", MAX_CONSECUTIVE_FAILURES);
                        actor.kill();
                        break;
                    }
                }
            }
        });

        // 4. 创建持仓轮询 Actor (每 3 秒)
        let position_polling = IbkrPositionPollingActor::spawn_link_with_mailbox(
            &actor_ref,
            IbkrPositionPollingActorArgs {
                client: args.client.clone(),
                income_pubsub: income_pubsub.clone(),
                interval_ms: POSITION_POLLING_INTERVAL_MS,
                symbols: args.client.conids().keys().cloned().collect(),
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "IBKR", "PositionPollingActor created");

        // 5. 创建市场状态轮询 Actor
        let status_polling = IbkrStatusPollingActor::spawn_link_with_mailbox(
            &actor_ref,
            IbkrStatusPollingActorArgs {
                client: args.client,
                income_pubsub: income_pubsub.clone(),
                interval_ms: STATUS_POLLING_INTERVAL_MS,
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "IBKR", "StatusPollingActor created");

        tracing::info!(exchange = "IBKR", "IbkrActor started");

        Ok(Self {
            public_ws,
            _position_polling: position_polling,
            _status_polling: status_polling,
        })
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
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to IbkrPublicWsActor");
        }
    }
}

impl Message<SubscribeBatch> for IbkrActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to IbkrPublicWsActor");
        }
    }
}

impl Message<Unsubscribe> for IbkrActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to IbkrPublicWsActor");
        }
    }
}

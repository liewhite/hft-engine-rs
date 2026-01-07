//! BinanceActor - Binance 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor、PrivateWsActor 和 EquityPollingActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到 IncomePubSub
//!
//! 架构:
//! BinanceActor (父)
//! ├── BinancePublicWsActor [spawn_link]
//! ├── BinancePrivateWsActor [spawn_link] (optional, 需要凭证)
//! │   └── BinanceListenKeyActor [spawn_link]
//! └── BinanceEquityPollingActor [spawn_link]

use super::equity_polling::{BinanceEquityPollingActor, BinanceEquityPollingActorArgs};
use super::private_ws::{BinancePrivateWsActor, BinancePrivateWsActorArgs};
use super::public_ws::{BinancePublicWsActor, BinancePublicWsActorArgs};
use crate::domain::{Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::{Subscribe, Unsubscribe};
use crate::exchange::ExchangeClient;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
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
    EquityPolling,
}

/// BinanceActor 初始化参数
pub struct BinanceActorArgs {
    /// 凭证（可选）
    pub credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// REST 基础 URL（用于 ListenKey）
    pub rest_base_url: String,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Exchange client（用于查询 equity）
    pub client: Arc<dyn ExchangeClient>,
}

/// BinanceActor - 父 Actor
pub struct BinanceActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<BinancePublicWsActor>,
    /// Private WebSocket Actor (可选，需要凭证)
    _private_ws: Option<ActorRef<BinancePrivateWsActor>>,
    /// Equity Polling Actor
    _equity_polling: ActorRef<BinanceEquityPollingActor>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorId, ChildKind>,
}

impl Actor for BinanceActor {
    type Args = BinanceActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let mut child_actors = HashMap::new();

        // 1. 创建 PublicWsActor (使用 spawn_link_with_mailbox)
        let public_ws = BinancePublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            BinancePublicWsActorArgs {
                income_pubsub: args.income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        tracing::info!(exchange = "Binance", "PublicWsActor created");

        // 2. 创建 PrivateWsActor (如果有凭证)
        let private_ws = if let Some(credentials) = args.credentials {
            let private_ws = BinancePrivateWsActor::spawn_link_with_mailbox(
                &actor_ref,
                BinancePrivateWsActorArgs {
                    credentials,
                    rest_base_url: args.rest_base_url,
                    income_pubsub: args.income_pubsub.clone(),
                    symbol_metas: args.symbol_metas.clone(),
                },
                mailbox::unbounded(),
            )
            .await;
            child_actors.insert(private_ws.id(), ChildKind::PrivateWs);
            tracing::info!(exchange = "Binance", "PrivateWsActor created");
            Some(private_ws)
        } else {
            None
        };

        // 3. 创建 EquityPollingActor
        let equity_polling = BinanceEquityPollingActor::spawn_link_with_mailbox(
            &actor_ref,
            BinanceEquityPollingActorArgs {
                client: args.client,
                income_pubsub: args.income_pubsub,
                interval_ms: 1000, // 每秒查询一次
            },
            mailbox::unbounded(),
        )
        .await;
        child_actors.insert(equity_polling.id(), ChildKind::EquityPolling);
        tracing::info!(exchange = "Binance", "EquityPollingActor created");

        tracing::info!(
            exchange = "Binance",
            has_private_ws = private_ws.is_some(),
            "BinanceActor started"
        );

        Ok(Self {
            public_ws,
            _private_ws: private_ws,
            _equity_polling: equity_polling,
            child_actors,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("BinanceActor stopped");
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
                tracing::error!(reason = ?reason, "BinancePublicWsActor died, shutting down");
            }
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "BinancePrivateWsActor died, shutting down");
            }
            Some(ChildKind::EquityPolling) => {
                tracing::error!(reason = ?reason, "BinanceEquityPollingActor died, shutting down");
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

impl Message<Subscribe> for BinanceActor {
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

impl Message<Unsubscribe> for BinanceActor {
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

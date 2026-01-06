//! BinanceActor - Binance 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor、PrivateWsActor 和 EquityPollingActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发送到 EventSink
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
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::{EventSink, Subscribe, Unsubscribe};
use crate::exchange::ExchangeClient;
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
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// Exchange client（用于查询 equity）
    pub client: Arc<dyn ExchangeClient>,
}

/// BinanceActor - 父 Actor
pub struct BinanceActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<BinancePublicWsActor>,
    /// Private WebSocket Actor (可选，需要凭证)
    private_ws: Option<ActorRef<BinancePrivateWsActor>>,
    /// Equity Polling Actor
    _equity_polling: ActorRef<BinanceEquityPollingActor>,

    /// 子 Actor ID -> Kind 映射
    child_actors: HashMap<ActorID, ChildKind>,
}

impl BinanceActor {
    /// 创建 BinanceActor 并返回 ActorRef
    ///
    /// 使用 PreparedActor 模式，在构造时 spawn_link 所有子 actors
    pub async fn new<P: Actor>(parent: &ActorRef<P>, args: BinanceActorArgs) -> ActorRef<Self> {
        // 1. 准备 actor，获取 actor_ref
        let prepared = PreparedActor::<Self>::new();
        let actor_ref = prepared.actor_ref().clone();

        // 2. 与父 actor 建立 link
        actor_ref.link(parent).await;

        let mut child_actors = HashMap::new();

        // 3. 创建 PublicWsActor
        let public_ws = spawn_link(
            &actor_ref,
            BinancePublicWsActor::new(BinancePublicWsActorArgs {
                event_sink: args.event_sink.clone(),
                symbol_metas: args.symbol_metas.clone(),
            }),
        )
        .await;
        child_actors.insert(public_ws.id(), ChildKind::PublicWs);
        tracing::info!(exchange = "Binance", "PublicWsActor created");

        // 4. 创建 PrivateWsActor (如果有凭证)
        let private_ws = if let Some(credentials) = args.credentials {
            let private_ws = spawn_link(
                &actor_ref,
                BinancePrivateWsActor::new(BinancePrivateWsActorArgs {
                    credentials,
                    rest_base_url: args.rest_base_url,
                    event_sink: args.event_sink.clone(),
                    symbol_metas: args.symbol_metas.clone(),
                }),
            )
            .await;
            child_actors.insert(private_ws.id(), ChildKind::PrivateWs);
            tracing::info!(exchange = "Binance", "PrivateWsActor created");
            Some(private_ws)
        } else {
            None
        };

        // 5. 创建 EquityPollingActor
        let equity_polling = spawn_link(
            &actor_ref,
            BinanceEquityPollingActor::new(BinanceEquityPollingActorArgs {
                client: args.client,
                event_sink: args.event_sink,
                interval_ms: 1000, // 每秒查询一次
            }),
        )
        .await;
        child_actors.insert(equity_polling.id(), ChildKind::EquityPolling);
        tracing::info!(exchange = "Binance", "EquityPollingActor created");

        // 6. 构造 actor 并 spawn
        let actor = Self {
            public_ws,
            private_ws,
            _equity_polling: equity_polling,
            child_actors,
        };

        prepared.spawn(actor);

        actor_ref
    }
}

impl Actor for BinanceActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::info!(
            exchange = "Binance",
            has_private_ws = self.private_ws.is_some(),
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
            }
            Some(ChildKind::PrivateWs) => {
                tracing::error!(reason = ?reason, "BinancePrivateWsActor died, shutting down");
            }
            Some(ChildKind::EquityPolling) => {
                tracing::error!(reason = ?reason, "BinanceEquityPollingActor died, shutting down");
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
        self.public_ws
            .tell(msg)
            .await
            .expect("Failed to forward Subscribe to PublicWsActor");
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
        self.public_ws
            .tell(msg)
            .await
            .expect("Failed to forward Unsubscribe to PublicWsActor");
    }
}

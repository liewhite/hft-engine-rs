//! BinanceActor - Binance 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor、PrivateWsActor 和 EquityPollingActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到 IncomePubSub
//! - 启动时查询持仓并推送到 IncomePubSub
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
use crate::domain::{Symbol, SymbolMeta, Timestamp};
use crate::engine::{CryptoStatusActor, CryptoStatusActorArgs, IncomePubSub};
use crate::exchange::binance::{BinanceClient, BinanceCredentials};
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::ExchangeClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 60_000;

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
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// BinanceActor - 父 Actor
pub struct BinanceActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<BinancePublicWsActor>,
}

impl Actor for BinanceActor {
    type Args = BinanceActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 0. 查询初始持仓并推送到 IncomePubSub
        if let Some(ref credentials) = args.credentials {
            let client = BinanceClient::new(Some(credentials.clone()))
                .expect("Failed to create BinanceClient");

            match client.fetch_positions().await {
                Ok(positions) => {
                    tracing::info!(
                        exchange = "Binance",
                        count = positions.len(),
                        "Fetched initial positions"
                    );

                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as Timestamp;

                    for position in positions {
                        tracing::info!(
                            exchange = "Binance",
                            symbol = %position.symbol,
                            size = position.size,
                            "Initial position loaded"
                        );
                        let event = IncomeEvent {
                            exchange_ts: now,
                            local_ts: now,
                            data: ExchangeEventData::Position(position),
                        };
                        if let Err(e) = args.income_pubsub.tell(Publish(event)).send().await {
                            tracing::error!(error = %e, "Failed to publish to IncomePubSub");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(exchange = "Binance", error = %e, "Failed to fetch initial positions");
                }
            }
        }

        // 1. 创建 PublicWsActor (使用 spawn_link_with_mailbox)
        let public_ws = BinancePublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            BinancePublicWsActorArgs {
                income_pubsub: args.income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "Binance", "PublicWsActor created");

        // 2. 创建 PrivateWsActor (如果有凭证)
        let has_private_ws = if let Some(credentials) = args.credentials {
            BinancePrivateWsActor::spawn_link_with_mailbox(
                &actor_ref,
                BinancePrivateWsActorArgs {
                    credentials,
                    rest_base_url: args.rest_base_url,
                    income_pubsub: args.income_pubsub.clone(),
                    symbol_metas: args.symbol_metas.clone(),
                    quote: args.quote.clone(),
                },
                mailbox::unbounded(),
            )
            .await;
            tracing::info!(exchange = "Binance", "PrivateWsActor created");
            true
        } else {
            false
        };

        // 3. 创建 EquityPollingActor
        BinanceEquityPollingActor::spawn_link_with_mailbox(
            &actor_ref,
            BinanceEquityPollingActorArgs {
                client: args.client,
                income_pubsub: args.income_pubsub.clone(),
                interval_ms: 1000, // 每秒查询一次
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "Binance", "EquityPollingActor created");

        // 4. 创建 CryptoStatusActor (加密货币 7x24 始终 Liquid)
        CryptoStatusActor::spawn_link_with_mailbox(
            &actor_ref,
            CryptoStatusActorArgs {
                exchange: crate::domain::Exchange::Binance,
                income_pubsub: args.income_pubsub,
                interval_ms: STATUS_BROADCAST_INTERVAL_MS,
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "Binance", "CryptoStatusActor created");

        tracing::info!(
            exchange = "Binance",
            has_private_ws,
            "BinanceActor started"
        );

        Ok(Self { public_ws })
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

impl Message<Subscribe> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 转发给 PublicWsActor
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to BinancePublicWsActor");
        }
    }
}

impl Message<SubscribeBatch> for BinanceActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 转发给 PublicWsActor
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to BinancePublicWsActor");
        }
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
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to BinancePublicWsActor");
        }
    }
}

//! HyperliquidActor - Hyperliquid 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到对应总线
//!
//! 一个 actor 覆盖 client 接入的全部 perp dex（共用一个钱包、一条公共 WS、一条私有 WS），
//! 品种归属由 client 的品种目录决定。
//!
//! 架构:
//! HyperliquidActor (父)
//! ├── HyperliquidPublicWsActor [spawn_link]
//! ├── HyperliquidPrivateWsActor [spawn_link] (optional, 需要凭证)
//! ├── EquityPollingActor [spawn_link] (optional, 需要凭证)
//! └── HyperliquidFundingFeePollingActor [spawn_link] (optional, 需要凭证)

use crate::actor_lifecycle::ChildGroup;
use super::funding_fee_polling::{
    HyperliquidFundingFeePollingActor, HyperliquidFundingFeePollingActorArgs,
};
use super::private_ws::{HyperliquidPrivateWsActor, HyperliquidPrivateWsActorArgs};
use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use crate::domain::ExchangeError;
use crate::exchange::equity_polling::{EquityPollingActor, EquityPollingActorArgs};
use crate::exchange::{CryptoStatusActor, CryptoStatusActorArgs};
use crate::messaging::{AccountPubSub, MarketPubSub};
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::hyperliquid::HyperliquidClient;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 5_000;

/// 资费结算轮询间隔 (毫秒)。与 Binance 侧一致：资费按小时结算，1 分钟一次绰绰有余。
const FUNDING_FEE_POLL_INTERVAL_MS: u64 = 60_000;

/// 账户净值轮询间隔 (毫秒)。一次 = spotClearinghouseState + 每个已接入 dex 一次
/// clearinghouseState（各约 2 权重）；2 秒一次、两个 dex 约 180 权重/分钟，
/// 与持仓对账、资金费拉取合计仍远低于 HL 每 IP 1200/分钟。陈旧守卫是 60 秒，间隔不影响它。
const EQUITY_POLL_INTERVAL_MS: u64 = 2_000;

/// HyperliquidActor 初始化参数
pub struct HyperliquidActorArgs {
    /// 装配处建好的 client（品种目录、凭证都在里面；与 ExchangeSetup 里的是同一个）
    pub client: Arc<HyperliquidClient>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_pubsub: ActorRef<MarketPubSub>,
    /// 账户私有事件总线（private WS / 账户轮询用）
    pub account_pubsub: ActorRef<AccountPubSub>,
}

/// HyperliquidActor - 父 Actor
pub struct HyperliquidActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<HyperliquidPublicWsActor>,
    /// 全部子 actor：谁 spawn 的谁负责等（见 [`crate::actor_lifecycle`]）
    children: ChildGroup,
}

impl Actor for HyperliquidActor {
    type Args = HyperliquidActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let client = args.client;
        let markets = client.markets().clone();
        // 有凭证 = client 有账户（同一个 Option 的两面），私有 WS 与各轮询都按它门控
        let private_feed = client.private_feed();

        // 1. 并发 spawn 两个 WS actor
        let mut children = ChildGroup::default();
        let public_ws = children.spawn::<HyperliquidPublicWsActor, _>(
            &actor_ref,
            "HyperliquidPublicWsActor",
            HyperliquidPublicWsActorArgs {
                market_pubsub: args.market_pubsub.clone(),
                markets: markets.clone(),
            },
        )
        .await;
        let private_ws_opt = if let Some((wallet_address, foreign_checked_at)) = private_feed.clone() {
            Some(
                children.spawn::<HyperliquidPrivateWsActor, _>(
                    &actor_ref,
                    "HyperliquidPrivateWsActor",
                    HyperliquidPrivateWsActorArgs {
                        wallet_address,
                        foreign_checked_at,
                        markets: markets.clone(),
                        account_pubsub: args.account_pubsub.clone(),
                    },
                )
                .await,
            )
        } else {
            None
        };
        let has_private_ws = private_ws_opt.is_some();

        // 2. 并发等 WS 全部完成；任一失败 → 向上传播（受控退出，不重试/不重连）
        let private_wait = async {
            if let Some(p) = &private_ws_opt {
                p.wait_for_startup_result().await
            } else {
                Ok(())
            }
        };
        let (public_r, private_r) = tokio::join!(
            public_ws.wait_for_startup_result(),
            private_wait,
        );
        public_r
            .map_err(|e| ExchangeError::Other(format!("HyperliquidPublicWsActor failed to start: {e}")))?;
        private_r
            .map_err(|e| ExchangeError::Other(format!("HyperliquidPrivateWsActor failed to start: {e}")))?;
        tracing::info!(exchange = "Hyperliquid", has_private_ws, dexes = ?markets.dexes(), "WS actors ready");

        // 2.5 账户轮询（净值、资费结算）：与私有 WS 一样按凭证门控。
        //     polling actor 的 on_start 只 attach_stream（无 IO），无需 wait_for_startup。
        if private_feed.is_some() {
            children.spawn::<EquityPollingActor, _>(
                &actor_ref,
                "EquityPollingActor",
                EquityPollingActorArgs {
                    client: client.clone(),
                    account_pubsub: args.account_pubsub.clone(),
                    interval_ms: EQUITY_POLL_INTERVAL_MS,
                },
            )
            .await;
            children.spawn::<HyperliquidFundingFeePollingActor, _>(
                &actor_ref,
                "HyperliquidFundingFeePollingActor",
                HyperliquidFundingFeePollingActorArgs {
                    client,
                    account_pubsub: args.account_pubsub.clone(),
                    interval_ms: FUNDING_FEE_POLL_INTERVAL_MS,
                },
            )
            .await;
        }

        // 3. 创建 CryptoStatusActor (加密货币 7x24 始终 Liquid)
        children.spawn::<CryptoStatusActor, _>(
            &actor_ref,
            "CryptoStatusActor",
            CryptoStatusActorArgs {
                exchange: crate::domain::Exchange::Hyperliquid,
                market_pubsub: args.market_pubsub.clone(),
                interval_ms: STATUS_BROADCAST_INTERVAL_MS,
            },
        )
        .await;
        tracing::info!(exchange = "Hyperliquid", "CryptoStatusActor created");

        tracing::info!(
            exchange = "Hyperliquid",
            has_private_ws,
            "HyperliquidActor started"
        );

        Ok(Self { public_ws, children })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // 谁 spawn 的谁负责等（见 actor_lifecycle 模块文档）
        self.children.shutdown().await;
        tracing::info!("HyperliquidActor stopped");
        Ok(())
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
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to HyperliquidPublicWsActor");
        }
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
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to HyperliquidPublicWsActor");
        }
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
        if let Err(e) = self.public_ws.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward message to HyperliquidPublicWsActor");
        }
    }
}

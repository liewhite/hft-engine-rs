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

use super::funding_fee_polling::{
    HyperliquidFundingFeePollingActor, HyperliquidFundingFeePollingActorArgs,
};
use super::private_ws::{HyperliquidPrivateWsActor, HyperliquidPrivateWsActorArgs};
use super::public_ws::{HyperliquidPublicWsActor, HyperliquidPublicWsActorArgs};
use crate::domain::{ExchangeError, Symbol, SymbolMeta};
use crate::engine::{CryptoStatusActor, CryptoStatusActorArgs, IncomePubSub};
use crate::exchange::client::{Subscribe, SubscribeBatch, Unsubscribe};
use crate::exchange::hyperliquid::{HyperliquidClient, HyperliquidCredentials};
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 5_000;

/// 资费结算轮询间隔 (毫秒)。与 Binance 侧一致：资费按小时结算，1 分钟一次绰绰有余。
const FUNDING_FEE_POLL_INTERVAL_MS: u64 = 60_000;

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
    /// Perp DEX 名称 ("" = 默认, "xyz" = 股票永续等)
    pub dex: String,
}

/// HyperliquidActor - 父 Actor
pub struct HyperliquidActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<HyperliquidPublicWsActor>,
}

impl Actor for HyperliquidActor {
    type Args = HyperliquidActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let income_pubsub = args.income_pubsub;

        // 凭证可用时构建 Arc<HyperliquidClient>，供 FundingFee polling 使用（与 BinanceActor 同构）。
        // 构建失败向上传播 → 启动期受控退出
        let hyperliquid_client: Option<Arc<HyperliquidClient>> = match args.credentials.as_ref() {
            Some(c) => Some(Arc::new(
                HyperliquidClient::new(args.quote.clone(), args.dex.clone(), Some(c.clone()))
                    .map_err(|e| {
                        ExchangeError::Other(format!("Failed to create HyperliquidClient: {e}"))
                    })?,
            )),
            None => None,
        };

        // 1. 并发 spawn 两个 WS actor
        let public_ws = HyperliquidPublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            HyperliquidPublicWsActorArgs {
                income_pubsub: income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
                dex: args.dex.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        let private_ws_opt = if let Some(credentials) = args.credentials {
            Some(
                HyperliquidPrivateWsActor::spawn_link_with_mailbox(
                    &actor_ref,
                    HyperliquidPrivateWsActorArgs {
                        wallet_address: credentials.wallet_address,
                        dex: args.dex.clone(),
                        income_pubsub: income_pubsub.clone(),
                    },
                    mailbox::unbounded(),
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
        tracing::info!(exchange = "Hyperliquid", has_private_ws, "WS actors ready");

        // 2.5 资费结算轮询：与私有 WS 一样按凭证门控（userFunding 要带 user 地址）。
        //     polling actor 的 on_start 只 attach_stream（无 IO），无需 wait_for_startup。
        if let Some(client) = hyperliquid_client {
            HyperliquidFundingFeePollingActor::spawn_link_with_mailbox(
                &actor_ref,
                HyperliquidFundingFeePollingActorArgs {
                    client,
                    income_pubsub: income_pubsub.clone(),
                    interval_ms: FUNDING_FEE_POLL_INTERVAL_MS,
                },
                mailbox::unbounded(),
            )
            .await;
        }

        // 3. 创建 CryptoStatusActor (加密货币 7x24 始终 Liquid)
        CryptoStatusActor::spawn_link_with_mailbox(
            &actor_ref,
            CryptoStatusActorArgs {
                exchange: crate::domain::Exchange::Hyperliquid,
                income_pubsub,
                interval_ms: STATUS_BROADCAST_INTERVAL_MS,
            },
            mailbox::unbounded(),
        )
        .await;
        tracing::info!(exchange = "Hyperliquid", "CryptoStatusActor created");

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

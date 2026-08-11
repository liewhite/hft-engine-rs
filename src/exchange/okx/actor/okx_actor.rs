//! OkxActor - OKX 交易所的父 Actor
//!
//! 职责:
//! - 管理 PublicWsActor 和 PrivateWsActor 子 actor
//! - 转发 Subscribe/Unsubscribe 到 PublicWsActor
//! - WsActors 直接解析消息并发布到对应总线
//!
//! 架构:
//! OkxActor (父)
//! ├── OkxPublicWsActor [spawn_link]
//! └── OkxPrivateWsActor [spawn_link] (optional, 需要凭证)

use crate::actor_lifecycle::ChildGroup;
use super::business_ws::{OkxBusinessWsActor, OkxBusinessWsActorArgs};
use super::greeks_polling::{OkxGreeksPollingActor, OkxGreeksPollingActorArgs};
use super::private_ws::{OkxPrivateWsActor, OkxPrivateWsActorArgs};
use super::public_ws::{OkxPublicWsActor, OkxPublicWsActorArgs};
use crate::domain::{ExchangeError, Symbol, SymbolMeta};
use crate::engine::{AccountPubSub, CryptoStatusActor, CryptoStatusActorArgs, MarketPubSub};
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe};
use crate::exchange::okx::{OkxClient, OkxCredentials};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 60_000;

/// Greeks REST 轮询间隔 (毫秒) — 每秒 1 次，官方限速 10/2s
const GREEKS_POLLING_INTERVAL_MS: u64 = 1000;

/// OkxActor 初始化参数
pub struct OkxActorArgs {
    /// 凭证（可选）
    pub credentials: Option<OkxCredentials>,
    /// REST client (用于 Greeks 轮询)
    pub client: Option<Arc<OkxClient>>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_pubsub: ActorRef<MarketPubSub>,
    /// 账户私有事件总线（private WS / 账户轮询用）
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// OkxActor - 父 Actor
pub struct OkxActor {
    /// Public WebSocket Actor
    public_ws: ActorRef<OkxPublicWsActor>,
    /// Business WebSocket Actor (K线)
    business_ws: ActorRef<OkxBusinessWsActor>,
    /// 全部子 actor：谁 spawn 的谁负责等（见 [`crate::actor_lifecycle`]）
    children: ChildGroup,
}

impl Actor for OkxActor {
    type Args = OkxActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {

        // 1. 并发 spawn 三个 WS actor（spawn 本身瞬间返回）
        let mut children = ChildGroup::default();
        let public_ws = children.spawn::<OkxPublicWsActor, _>(
            &actor_ref,
            "OkxPublicWsActor",
            OkxPublicWsActorArgs {
                market_pubsub: args.market_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
        )
        .await;
        let business_ws = children.spawn::<OkxBusinessWsActor, _>(
            &actor_ref,
            "OkxBusinessWsActor",
            OkxBusinessWsActorArgs {
                market_pubsub: args.market_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
        )
        .await;
        let private_ws_opt = if let Some(credentials) = args.credentials {
            Some(
                children.spawn::<OkxPrivateWsActor, _>(
                    &actor_ref,
                    "OkxPrivateWsActor",
                    OkxPrivateWsActorArgs {
                        credentials,
                        account_pubsub: args.account_pubsub.clone(),
                        symbol_metas: args.symbol_metas,
                    },
                )
                .await,
            )
        } else {
            None
        };
        let has_private_ws = private_ws_opt.is_some();

        // 2. 并发等三个 WS 全部完成；任一失败 → 向上传播（启动期受控退出，不重试）
        let private_wait = async {
            if let Some(p) = &private_ws_opt {
                p.wait_for_startup_result().await
            } else {
                Ok(())
            }
        };
        let (public_r, business_r, private_r) = tokio::join!(
            public_ws.wait_for_startup_result(),
            business_ws.wait_for_startup_result(),
            private_wait,
        );
        public_r.map_err(|e| ExchangeError::Other(format!("OkxPublicWsActor failed to start: {e}")))?;
        business_r
            .map_err(|e| ExchangeError::Other(format!("OkxBusinessWsActor failed to start: {e}")))?;
        private_r
            .map_err(|e| ExchangeError::Other(format!("OkxPrivateWsActor failed to start: {e}")))?;
        tracing::info!(exchange = "OKX", has_private_ws, "WS actors ready");

        // 3. polling actor 的 on_start 仅 attach_stream，省略 wait
        //
        // Greeks 轮询要签名，故按**凭证**门控：装配处只在有凭证时才给 client
        // （见 assembly::setup_okx），所以这里 `Some` 即"可轮询"，不必再判一次 bool。
        // 无凭证时不起它 —— 否则 non-auth 模式会每秒失败刷日志。
        if let Some(client) = args.client {
            children.spawn::<OkxGreeksPollingActor, _>(
                &actor_ref,
                "OkxGreeksPollingActor",
                OkxGreeksPollingActorArgs {
                    client,
                    account_pubsub: args.account_pubsub.clone(),
                    interval_ms: GREEKS_POLLING_INTERVAL_MS,
                },
            )
            .await;
        }
        children.spawn::<CryptoStatusActor, _>(
            &actor_ref,
            "CryptoStatusActor",
            CryptoStatusActorArgs {
                exchange: crate::domain::Exchange::OKX,
                market_pubsub: args.market_pubsub.clone(),
                interval_ms: STATUS_BROADCAST_INTERVAL_MS,
            },
        )
        .await;

        tracing::info!(
            exchange = "OKX",
            has_private_ws,
            "OkxActor started"
        );

        Ok(Self { public_ws, business_ws, children })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // 谁 spawn 的谁负责等（见 actor_lifecycle 模块文档）
        self.children.shutdown().await;
        tracing::info!("OkxActor stopped");
        Ok(())
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
        match &msg.kind {
            SubscriptionKind::Candle { .. } => {
                if let Err(e) = self.business_ws.tell(msg).send().await {
                    tracing::error!(error = %e, "Failed to forward message to OkxBusinessWsActor");
                }
            }
            _ => {
                if let Err(e) = self.public_ws.tell(msg).send().await {
                    tracing::error!(error = %e, "Failed to forward message to OkxPublicWsActor");
                }
            }
        }
    }
}

impl Message<SubscribeBatch> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 按目标 actor 拆分
        let mut public_kinds = Vec::new();
        let mut business_kinds = Vec::new();

        for kind in msg.kinds {
            match &kind {
                SubscriptionKind::Candle { .. } => business_kinds.push(kind),
                _ => public_kinds.push(kind),
            }
        }

        if !public_kinds.is_empty() {
            if let Err(e) = self.public_ws.tell(SubscribeBatch { kinds: public_kinds }).send().await {
                tracing::error!(error = %e, "Failed to forward message to OkxPublicWsActor");
            }
        }
        if !business_kinds.is_empty() {
            if let Err(e) = self.business_ws.tell(SubscribeBatch { kinds: business_kinds }).send().await {
                tracing::error!(error = %e, "Failed to forward message to OkxBusinessWsActor");
            }
        }
    }
}

impl Message<Unsubscribe> for OkxActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        match &msg.kind {
            SubscriptionKind::Candle { .. } => {
                if let Err(e) = self.business_ws.tell(msg).send().await {
                    tracing::error!(error = %e, "Failed to forward message to OkxBusinessWsActor");
                }
            }
            _ => {
                if let Err(e) = self.public_ws.tell(msg).send().await {
                    tracing::error!(error = %e, "Failed to forward message to OkxPublicWsActor");
                }
            }
        }
    }
}

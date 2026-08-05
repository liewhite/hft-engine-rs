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
use super::funding_fee_polling::{BinanceFundingFeePollingActor, BinanceFundingFeePollingActorArgs};
use super::private_ws::{BinancePrivateWsActor, BinancePrivateWsActorArgs};
use super::public_ws::{BinancePublicWsActor, BinancePublicWsActorArgs};
use crate::domain::{ExchangeError, Symbol, SymbolMeta};
use crate::engine::{CryptoStatusActor, CryptoStatusActorArgs, IncomePubSub};
use crate::exchange::binance::{
    BinanceClient, BinanceCredentials, WS_MARKET_URL, WS_PUBLIC_HIGH_FREQ_URL,
};
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe};
use crate::exchange::ExchangeClient;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
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
    /// 盘口高频 WebSocket（/public/ws：bookTicker、depth）
    public_ws: ActorRef<BinancePublicWsActor>,
    /// 常规市场 WebSocket（/market/ws：markPrice、kline、ticker、aggTrade）
    market_ws: ActorRef<BinancePublicWsActor>,
}

/// 公共 WS 目标（Binance 迁移后按路由路径分流，两条连接）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WsTarget {
    /// /public/ws：盘口高频数据（bookTicker、depth）
    PublicHighFreq,
    /// /market/ws：常规市场数据（markPrice、kline、ticker）+ aggTrade
    Market,
}

impl WsTarget {
    /// 该目标对应的 WS 端点。kind -> URL 的映射由此处与 [`pick_ws_target`] 共同构成
    /// 单一来源：spawn 子 actor 与线路测试都经此取 URL，避免两边各写一份而漂移。
    pub(crate) fn url(self) -> &'static str {
        match self {
            WsTarget::PublicHighFreq => WS_PUBLIC_HIGH_FREQ_URL,
            WsTarget::Market => WS_MARKET_URL,
        }
    }
}

/// 按订阅 kind 选择落到哪条公共 WS
///
/// `Trades` 走 /market/ws —— aggTrade 归属该端点，订到 /public/ws 会被 ack 但永不推数据
/// （静默无数据，见 [`crate::exchange::binance::WS_PUBLIC_HIGH_FREQ_URL`] 的注释）。
pub(crate) fn pick_ws_target(kind: &SubscriptionKind) -> WsTarget {
    match kind {
        SubscriptionKind::BBO { .. } => WsTarget::PublicHighFreq,
        SubscriptionKind::Trades { .. }
        | SubscriptionKind::FundingRate { .. }
        | SubscriptionKind::MarkPrice { .. }
        | SubscriptionKind::IndexPrice { .. }
        | SubscriptionKind::Candle { .. } => WsTarget::Market,
    }
}

impl BinanceActor {
    fn ws_for(&self, target: WsTarget) -> &ActorRef<BinancePublicWsActor> {
        match target {
            WsTarget::PublicHighFreq => &self.public_ws,
            WsTarget::Market => &self.market_ws,
        }
    }
}

impl Actor for BinanceActor {
    type Args = BinanceActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 凭证可用时构建 Arc<BinanceClient>，供 FundingFee polling 使用。
        // 初始持仓查询已上移至 ManagerActor::add_strategies_batch（executor 注册后），不在此处。
        // 构建失败向上传播 → 启动期受控退出
        let binance_client: Option<Arc<BinanceClient>> = match args.credentials.as_ref() {
            Some(c) => Some(Arc::new(
                BinanceClient::new(args.quote.clone(), Some(c.clone()))
                    .map_err(|e| ExchangeError::Other(format!("Failed to create BinanceClient: {e}")))?,
            )),
            None => None,
        };

        // 1. 先并发 spawn 所有 actor（spawn 本身是 instant，on_start 异步跑）
        let public_ws = BinancePublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            BinancePublicWsActorArgs {
                url: WsTarget::PublicHighFreq.url().to_string(),
                income_pubsub: args.income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        let market_ws = BinancePublicWsActor::spawn_link_with_mailbox(
            &actor_ref,
            BinancePublicWsActorArgs {
                url: WsTarget::Market.url().to_string(),
                income_pubsub: args.income_pubsub.clone(),
                symbol_metas: args.symbol_metas.clone(),
                quote: args.quote.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        let private_ws_opt = if let Some(credentials) = args.credentials {
            Some(
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
                .await,
            )
        } else {
            None
        };
        let has_private_ws = private_ws_opt.is_some();

        // 2. 并发等三个 WS actor 全部 on_start 完成；任一失败 → 向上传播（启动期受控退出，不重试），避免"假就绪"窗口
        let private_wait = async {
            if let Some(p) = &private_ws_opt {
                p.wait_for_startup_result().await
            } else {
                Ok(())
            }
        };
        let (public_r, market_r, private_r) = tokio::join!(
            public_ws.wait_for_startup_result(),
            market_ws.wait_for_startup_result(),
            private_wait,
        );
        public_r.map_err(|e| {
            ExchangeError::Other(format!("BinancePublicWsActor (/public/ws) failed to start: {e}"))
        })?;
        market_r.map_err(|e| {
            ExchangeError::Other(format!("BinancePublicWsActor (/market/ws) failed to start: {e}"))
        })?;
        private_r.map_err(|e| {
            ExchangeError::Other(format!("BinancePrivateWsActor failed to start: {e}"))
        })?;
        tracing::info!(exchange = "Binance", has_private_ws, "WS actors ready");

        // 3. polling actor 的 on_start 只 attach_stream（无 IO），wait_for_startup 是 no-op，省略
        //
        // 私有轮询与私有 WS 一样按凭证门控：无凭证时只订阅公共行情（non-auth 模式），
        // 否则 equity 轮询会每秒失败一次、刷满日志（既是噪音，也会掩盖真实告警）。
        if has_private_ws {
            BinanceEquityPollingActor::spawn_link_with_mailbox(
                &actor_ref,
                BinanceEquityPollingActorArgs {
                    client: args.client,
                    income_pubsub: args.income_pubsub.clone(),
                    interval_ms: 1000,
                },
                mailbox::unbounded(),
            )
            .await;
        }
        if let Some(client) = binance_client {
            BinanceFundingFeePollingActor::spawn_link_with_mailbox(
                &actor_ref,
                BinanceFundingFeePollingActorArgs {
                    client,
                    income_pubsub: args.income_pubsub.clone(),
                    interval_ms: 60_000,
                },
                mailbox::unbounded(),
            )
            .await;
        }
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

        tracing::info!(
            exchange = "Binance",
            has_private_ws,
            "BinanceActor started"
        );

        Ok(Self { public_ws, market_ws })
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
        let target = self.ws_for(pick_ws_target(&msg.kind));
        if let Err(e) = target.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward Subscribe");
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
        // 按目标 WS 拆分批次，避免把 markPrice 流发到 /public/ws 被拒
        let mut public_kinds = Vec::new();
        let mut market_kinds = Vec::new();
        for kind in msg.kinds {
            match pick_ws_target(&kind) {
                WsTarget::PublicHighFreq => public_kinds.push(kind),
                WsTarget::Market => market_kinds.push(kind),
            }
        }

        if !public_kinds.is_empty() {
            if let Err(e) = self
                .public_ws
                .tell(SubscribeBatch { kinds: public_kinds })
                .send()
                .await
            {
                tracing::error!(error = %e, "Failed to forward SubscribeBatch to public_ws");
            }
        }
        if !market_kinds.is_empty() {
            if let Err(e) = self
                .market_ws
                .tell(SubscribeBatch { kinds: market_kinds })
                .send()
                .await
            {
                tracing::error!(error = %e, "Failed to forward SubscribeBatch to market_ws");
            }
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
        let target = self.ws_for(pick_ws_target(&msg.kind));
        if let Err(e) = target.tell(msg).send().await {
            tracing::error!(error = %e, "Failed to forward Unsubscribe");
        }
    }
}

//! 交易所装配层（组合根）：每所一个 `setup_*`，产出 [`ExchangeSetup`] 交给
//! `ManagerActorArgs::exchanges` —— manager 对"有哪些所"彻底无知，只做收集与循环。
//!
//! 装配天然分两阶段：**建 client** 必须先于 symbol metas 预加载（预加载要用它），
//! **spawn WS actor 集**必须后于预加载（actor 要用 metas）。每所把两阶段封进一个
//! `setup_*` 函数：阶段 1 立即执行产出 client，阶段 2 以闭包延迟到 metas 就绪。
//! IBKR 的构造是异步的（要打网关）、配置形状也不同（无 quote、另带 snapshot 轮询
//! 配置）—— 差异全部封在它自己的 setup 内。

use super::manager::ManagerActor;
use super::{AccountPubSub, MarketPubSub};
use crate::actor_lifecycle::ChildStop;
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{BinanceActor, BinanceActorArgs, BinanceClient, BinanceCredentials, REST_BASE_URL};
use crate::exchange::hyperliquid::{HyperliquidActor, HyperliquidActorArgs, HyperliquidClient, HyperliquidCredentials};
use crate::exchange::ibkr::{IbkrActor, IbkrActorArgs, IbkrClient, IbkrCredentials, IbkrSnapshotConfig};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxClient, OkxCredentials};
use crate::exchange::{AccountClient, ExchangeAccess, ExchangeActorOps, ExchangeClient, SubscriptionKind};
use kameo::actor::{ActorRef, Spawn};
use kameo::mailbox;
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// 交易所装配：每所一个 setup_*，manager 只做收集与循环
// ============================================================================

/// 交易所 WS actor 装配的执行环境（metas 预加载完成后可用；由 manager 的 on_start 构造）
pub struct SpawnCtx {
    pub(crate) manager: ActorRef<ManagerActor>,
    pub(crate) market_pubsub: ActorRef<MarketPubSub>,
    /// 账户私有事件总线（实盘适配层 + 本地柜台共用，见 [`AccountPubSub`]）
    pub(crate) account_pubsub: ActorRef<AccountPubSub>,
    /// 该所的 symbol -> meta
    pub(crate) symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// 装配产物：类型擦除的操作接口 + 停机句柄。
///
/// 句柄必须在这里造：出了这个闭包，actor 就只剩 `Box<dyn ExchangeActorOps>`，
/// 拿不到具体的 `ActorRef<A>`，也就造不出"停它并等它收尾完"的闭包了。
pub(crate) type SpawnFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(Box<dyn ExchangeActorOps>, ChildStop), ExchangeError>>
            + Send,
    >,
>;

/// 单个交易所的装配产物（两阶段划分的理由见模块文档）。
///
/// **加一个新交易所只需**：本文件写一个 `setup_*`、各 bin 的装配处加一行 push ——
/// manager 对"有哪些所"彻底无知（`ManagerActorArgs` 收 `Vec<ExchangeSetup>`）。
pub struct ExchangeSetup {
    pub(crate) exchange: Exchange,
    pub(crate) client: Arc<dyn ExchangeClient>,
    /// 账户私有能力。`None` = 只接公共行情。
    ///
    /// "有没有账户"就由这个 `Option` 表达，不另存 bool（见 `docs/architecture.md` P2）：
    /// 只有 [`ExchangeAccess::has_credentials`] 为真时才装进来，因此下游拿到 `Some`
    /// 即可直接下单/对账，不必再问一次"配了凭证没有"。
    pub(crate) account: Option<Arc<dyn AccountClient>>,
    /// 能力查询：该所适配层是否实现某种公共订阅（知识在各适配层的
    /// `supports_subscription`，随 setup 携带 —— 投产校验用，无集中能力表）
    pub(crate) supports: fn(&SubscriptionKind) -> bool,
    /// 延迟执行的 actor 装配：spawn + 等 on_start 完成，返回类型擦除的操作接口。
    /// 各 setup 的 future 由 on_start 并发 join —— spawn 瞬间返回，等待是并发的。
    pub(crate) spawn_actor: Box<dyn FnOnce(SpawnCtx) -> SpawnFuture + Send>,
}

pub fn setup_binance(access: ExchangeAccess<BinanceCredentials>) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(BinanceClient::new(
        access.quote.clone(),
        access.credentials.clone(),
    )?);
    let account: Option<Arc<dyn AccountClient>> =
        access.has_credentials().then(|| client.clone() as Arc<dyn AccountClient>);
    Ok(ExchangeSetup {
        exchange: Exchange::Binance,
        client: client.clone(),
        account,
        supports: crate::exchange::binance::supports_subscription,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = BinanceActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    BinanceActorArgs {
                        credentials: access.credentials,
                        symbol_metas: ctx.symbol_metas,
                        rest_base_url: REST_BASE_URL.to_string(),
                        market_pubsub: ctx.market_pubsub,
                        account_pubsub: ctx.account_pubsub,
                        quote: access.quote,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor.wait_for_startup_result().await.map_err(|e| {
                    ExchangeError::Other(format!("BinanceActor failed to start: {e}"))
                })?;
                let stop = ChildStop::new("BinanceActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

pub fn setup_okx(access: ExchangeAccess<OkxCredentials>) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(OkxClient::new(access.quote.clone(), access.credentials.clone())?);
    let account: Option<Arc<dyn AccountClient>> =
        access.has_credentials().then(|| client.clone() as Arc<dyn AccountClient>);
    Ok(ExchangeSetup {
        exchange: Exchange::OKX,
        client: client.clone(),
        account,
        supports: crate::exchange::okx::supports_subscription,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = OkxActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    OkxActorArgs {
                        credentials: access.credentials,
                        client: Some(client),
                        symbol_metas: ctx.symbol_metas,
                        market_pubsub: ctx.market_pubsub,
                        account_pubsub: ctx.account_pubsub,
                        quote: access.quote,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor
                    .wait_for_startup_result()
                    .await
                    .map_err(|e| ExchangeError::Other(format!("OkxActor failed to start: {e}")))?;
                let stop = ChildStop::new("OkxActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

pub fn setup_hyperliquid(
    access: ExchangeAccess<HyperliquidCredentials>,
) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(HyperliquidClient::new(
        access.quote.clone(),
        access.dex.clone(),
        access.credentials.clone(),
    )?);
    let account: Option<Arc<dyn AccountClient>> =
        access.has_credentials().then(|| client.clone() as Arc<dyn AccountClient>);
    Ok(ExchangeSetup {
        exchange: Exchange::Hyperliquid,
        client: client.clone(),
        account,
        supports: crate::exchange::hyperliquid::supports_subscription,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = HyperliquidActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    HyperliquidActorArgs {
                        credentials: access.credentials,
                        symbol_metas: ctx.symbol_metas,
                        market_pubsub: ctx.market_pubsub,
                        account_pubsub: ctx.account_pubsub,
                        quote: access.quote,
                        dex: access.dex,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor.wait_for_startup_result().await.map_err(|e| {
                    ExchangeError::Other(format!("HyperliquidActor failed to start: {e}"))
                })?;
                let stop = ChildStop::new("HyperliquidActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}

/// IBKR 的构造是异步的（要打网关），且配置形状与其他三所不同（无 quote、另带
/// snapshot 轮询配置）—— 差异封在本函数内，manager 不感知。
pub async fn setup_ibkr(
    cred: IbkrCredentials,
    snapshot: Option<IbkrSnapshotConfig>,
) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(IbkrClient::new(&cred).await?);
    let auth = client.auth();
    let conids = client.conids().clone();
    Ok(ExchangeSetup {
        exchange: Exchange::IBKR,
        client: client.clone(),
        // IBKR 的 client 只在有凭证时才构建（`IbkrClient::new` 要打网关鉴权），
        // 走到这里必然有账户
        account: Some(client.clone()),
        supports: crate::exchange::ibkr::supports_subscription,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                let actor = IbkrActor::spawn_link_with_mailbox(
                    &ctx.manager,
                    IbkrActorArgs {
                        auth,
                        market_pubsub: ctx.market_pubsub,
                        account_pubsub: ctx.account_pubsub,
                        conids,
                        client,
                        snapshot,
                    },
                    mailbox::unbounded(),
                )
                .await;
                actor
                    .wait_for_startup_result()
                    .await
                    .map_err(|e| ExchangeError::Other(format!("IbkrActor failed to start: {e}")))?;
                let stop = ChildStop::new("IbkrActor", actor.clone());
                Ok((Box::new(actor) as Box<dyn ExchangeActorOps>, stop))
            })
        }),
    })
}


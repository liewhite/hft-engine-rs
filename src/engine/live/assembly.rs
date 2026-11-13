//! 交易所装配层（组合根）：每所一个 `setup_*`，产出 [`ExchangeSetup`] 交给
//! `ManagerArgs::exchanges` —— manager 对"有哪些所"彻底无知，只做收集与循环。
//!
//! 各 bin 不自己挨个装配：配置里的 `exchanges` 段解析成 [`ExchangesConfig`]，交给
//! [`setup_all`] 一次装好（#757）。各所的公共能力（如已结算资金费）随装配产物交给 manager，
//! bin 经 `ManagerHandle` 取用，不另建 client。
//!
//! 装配天然分两阶段：**建 client** 必须先于 symbol metas 预加载（预加载要用它），
//! **起交易所节点**必须后于预加载（节点要用 metas）。每所把两阶段封进一个
//! `setup_*` 函数：阶段 1 立即执行产出 client，阶段 2 以闭包延迟到 metas 就绪。
//! IBKR 的构造是异步的（要打网关）、配置形状也不同（无 quote、另带 snapshot 轮询
//! 配置）—— 差异全部封在它自己的 setup 内。

use super::AccountBus;
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::engine::NewClientOrderId;
use crate::exchange::binance::{
    BinanceAccount, BinanceArgs, BinanceClient, BinanceCredentials, BinanceNode,
};
use crate::exchange::hyperliquid::{
    HyperliquidAccess, HyperliquidAccount, HyperliquidArgs, HyperliquidClient, HyperliquidNode,
};
use crate::exchange::ibkr::{IbkrArgs, IbkrClient, IbkrCredentials, IbkrNode, IbkrSnapshotConfig};
use crate::exchange::okx::{OkxAccount, OkxArgs, OkxClient, OkxCredentials, OkxNode};
use crate::exchange::okx::{OkxGreeksPoller, GREEKS_POLLING_INTERVAL_MS};
use crate::exchange::{
    AccountClient, AccountStream, ExchangeAccess, ExchangeClient, ExchangeHandle,
    SettledFundingSource, SubscriptionKind,
};
use crate::messaging::MarketBus;
use crate::tasks::{Group, GroupClosed};
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// 交易所装配：每所一个 setup_*，manager 只做收集与循环
// ============================================================================

/// 交易所节点装配的执行环境（metas 预加载完成后可用；由 manager 构造时建好）
pub struct SpawnCtx {
    /// manager 的生产者组：交易所节点挂在这里，停机时先于消费者停
    pub(crate) producers: Group,
    pub(crate) market_bus: MarketBus,
    /// 账户私有事件总线（实盘适配层 + 本地柜台共用，见 [`AccountBus`]）
    pub(crate) account_bus: AccountBus,
    /// 该所的 symbol -> meta
    pub(crate) symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// 装配产物：交易所节点的句柄（订阅 / 退订行情用）。节点的生死归任务树管，句柄只是地址。
pub(crate) type SpawnFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ExchangeHandle, ExchangeError>> + Send>,
>;

/// 单个交易所的装配产物（两阶段划分的理由见模块文档）。
///
/// **加一个新交易所只需**：本文件写一个 `setup_*`、各 bin 的装配处加一行 push ——
/// manager 对"有哪些所"彻底无知（`ManagerArgs` 收 `Vec<ExchangeSetup>`）。
pub struct ExchangeSetup {
    pub(crate) exchange: Exchange,
    pub(crate) client: Arc<dyn ExchangeClient>,
    /// 账户私有能力。`None` = 只接公共行情。
    ///
    /// "有没有账户"就由这个 `Option` 表达，不另存 bool（见 `docs/architecture.md` P2）：
    /// 只有配了凭证时才构造各所的账户类型装进来，因此下游拿到 `Some` 即可直接下单/对账，
    /// 不必再问一次"配了凭证没有"。
    pub(crate) account: Option<Arc<dyn AccountClient>>,
    /// 能力查询：该所适配层是否实现某种公共订阅（知识在各适配层的
    /// `supports_subscription`，随 setup 携带 —— 投产校验用，无集中能力表）
    pub(crate) supports: fn(&SubscriptionKind) -> bool,
    /// 生成本引擎在该所的订单号（格式是适配层的线路知识，见 [`crate::exchange::CliOrderIdFormat`]；
    /// 框架只拿不透明的字符串）
    pub(crate) new_client_order_id: NewClientOrderId,
    /// 公共的已结算资金费率来源（该所适配层提供时）。交给 manager，bin 经
    /// `ManagerHandle::settled_funding` 取用，不为它另建 client
    pub(crate) settled_funding: Option<Arc<dyn SettledFundingSource>>,
    /// 该所能起的账户类数据流，策略声明需要时投产编排才起（见 [`AccountStream`]）。
    /// 只在配了凭证时登记 —— 没凭证就没有账户可读，键不在即"不提供"
    pub(crate) account_streams: HashMap<AccountStream, StartAccountStream>,
    /// 延迟执行的节点装配：在生产者组下起节点并等它构造完成，返回它的句柄。
    /// 各 setup 的 future 由 manager 构造时并发 join。
    pub(crate) spawn_actor: Box<dyn FnOnce(SpawnCtx) -> SpawnFuture + Send>,
}

/// 起一种账户类数据流（见 [`AccountStream`]）：在给定分组下起它，读数发到账户总线
pub(crate) type StartAccountStream =
    Box<dyn Fn(&Group, AccountBus) -> Result<(), GroupClosed> + Send + Sync>;

/// 节点启动失败转成装配错误（带上根因链）
fn start_failed(name: &str, e: anyhow::Error) -> ExchangeError {
    ExchangeError::new(format!("{name} failed to start: {e:#}"))
}

pub fn setup_binance(
    access: ExchangeAccess<BinanceCredentials>,
) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(BinanceClient::new(access.quote.clone())?);
    let account = access
        .credentials
        .map(|c| Arc::new(BinanceAccount::new(&client, c)));
    Ok(ExchangeSetup {
        exchange: Exchange::Binance,
        client: client.clone(),
        account: account.clone().map(|a| a as Arc<dyn AccountClient>),
        supports: crate::exchange::binance::supports_subscription,
        new_client_order_id: || crate::exchange::binance::CLIENT_ORDER_IDS.generate(),
        settled_funding: Some(client),
        account_streams: HashMap::new(),
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                BinanceNode::spawn(
                    &ctx.producers,
                    BinanceArgs {
                        account,
                        market_bus: ctx.market_bus,
                        account_bus: ctx.account_bus,
                        quote: access.quote,
                    },
                )
                .await
                .map_err(|e| start_failed("BinanceNode", e))
            })
        }),
    })
}

pub fn setup_okx(access: ExchangeAccess<OkxCredentials>) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(OkxClient::new(access.quote.clone())?);
    let account = access
        .credentials
        .map(|c| OkxAccount::new(client.clone(), c).map(Arc::new))
        .transpose()?;
    let mut account_streams: HashMap<AccountStream, StartAccountStream> = HashMap::new();
    if let Some(a) = account.clone() {
        account_streams.insert(
            AccountStream::Greeks,
            Box::new(move |group: &Group, account_bus: AccountBus| {
                OkxGreeksPoller::spawn(group, a.clone(), account_bus, GREEKS_POLLING_INTERVAL_MS)
                    .map(|_| ())
            }),
        );
    }
    Ok(ExchangeSetup {
        exchange: Exchange::OKX,
        client: client.clone(),
        account: account.clone().map(|a| a as Arc<dyn AccountClient>),
        supports: crate::exchange::okx::supports_subscription,
        new_client_order_id: || crate::exchange::okx::CLIENT_ORDER_IDS.generate(),
        settled_funding: Some(client),
        account_streams,
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                OkxNode::spawn(
                    &ctx.producers,
                    OkxArgs {
                        account,
                        symbol_metas: ctx.symbol_metas,
                        market_bus: ctx.market_bus,
                        account_bus: ctx.account_bus,
                        quote: access.quote,
                    },
                )
                .await
                .map_err(|e| start_failed("OkxNode", e))
            })
        }),
    })
}

/// 构造是异步的：client 启动时要拉各 dex 的 meta 建品种目录（见 [`HyperliquidClient::connect`]），
/// 有凭证时再查账户模式（见 [`HyperliquidAccount::connect`]）；一个 client 覆盖配置里的全部 dex。
pub async fn setup_hyperliquid(access: HyperliquidAccess) -> Result<ExchangeSetup, ExchangeError> {
    let client = Arc::new(HyperliquidClient::connect(access.quote, access.dexes).await?);
    let account = match access.credentials {
        Some(c) => Some(Arc::new(
            HyperliquidAccount::connect(client.clone(), c).await?,
        )),
        None => None,
    };
    Ok(ExchangeSetup {
        exchange: Exchange::Hyperliquid,
        client: client.clone(),
        account: account.clone().map(|a| a as Arc<dyn AccountClient>),
        supports: crate::exchange::hyperliquid::supports_subscription,
        new_client_order_id: || crate::exchange::hyperliquid::CLIENT_ORDER_IDS.generate(),
        settled_funding: Some(client.clone()),
        account_streams: HashMap::new(),
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                HyperliquidNode::spawn(
                    &ctx.producers,
                    HyperliquidArgs {
                        client,
                        account,
                        market_bus: ctx.market_bus,
                        account_bus: ctx.account_bus,
                    },
                )
                .await
                .map_err(|e| start_failed("HyperliquidNode", e))
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
        new_client_order_id: || crate::exchange::ibkr::CLIENT_ORDER_IDS.generate(),
        settled_funding: None,
        account_streams: HashMap::new(),
        spawn_actor: Box::new(move |ctx| {
            Box::pin(async move {
                IbkrNode::spawn(
                    &ctx.producers,
                    IbkrArgs {
                        auth,
                        market_bus: ctx.market_bus,
                        account_bus: ctx.account_bus,
                        conids,
                        client,
                        snapshot,
                    },
                )
                .await
                .map_err(|e| start_failed("IbkrNode", e))
            })
        }),
    })
}

/// 配置里的 `exchanges` 段：配了哪所就接哪所，不认识的键报错。各 bin 共用这一份，交给
/// [`setup_all`] 装配。IBKR 的接入形状不同（网关凭证 + 快照配置）、目前没有 bin 用，仍用
/// [`setup_ibkr`] 单独装配。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangesConfig {
    #[serde(default)]
    pub binance: Option<ExchangeAccess<BinanceCredentials>>,
    #[serde(default)]
    pub hyperliquid: Option<HyperliquidAccess>,
    #[serde(default)]
    pub okx: Option<ExchangeAccess<OkxCredentials>>,
}

/// 按配置装配全部所（加一所：[`ExchangesConfig`] 加一个字段、这里加一段）
pub async fn setup_all(cfg: &ExchangesConfig) -> Result<Vec<ExchangeSetup>, ExchangeError> {
    let mut setups = Vec::new();
    if let Some(a) = &cfg.binance {
        setups.push(setup_binance(a.clone())?);
    }
    if let Some(a) = &cfg.hyperliquid {
        setups.push(setup_hyperliquid(a.clone()).await?);
    }
    if let Some(a) = &cfg.okx {
        setups.push(setup_okx(a.clone())?);
    }
    Ok(setups)
}

//! OkxNode - OKX 交易所节点
//!
//! 职责:
//! - 管理公共/私有/业务 WS 和各轮询子任务
//! - 把订阅命令按 kind 路由：Candle 到 Business WS，其余到公共 WS
//! - WS 组件直接解析消息并发布到对应总线
//!
//! 树:
//! OkxNode
//! ├── OkxPublicWs（第一条订阅时才连）
//! ├── OkxBusinessWs（K线，第一条 K 线订阅时才连）
//! ├── OkxPrivateWs（需要凭证）
//! ├── EquityPoller（需要凭证；净值）
//! ├── FundingFeePoller（需要凭证）
//! └── CryptoStatus
//!
//! 希腊值轮询不在节点下：策略声明要才起（见 `crate::exchange::AccountStream`）。

use super::business_ws::OkxBusinessWs;
use super::private_ws::OkxPrivateWs;
use super::public_ws::OkxPublicWs;
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::client::{ExchangeCmd, ExchangeHandle, SubscriptionKind};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::funding_fee_polling::{FundingFeePoller, FUNDING_FEE_POLL_INTERVAL_MS};
use crate::exchange::lazy_conn::LazyConn;
use crate::exchange::okx::OkxAccount;
use crate::exchange::CryptoStatus;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Group, Inbox, Scope, Task};
use std::collections::HashMap;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 60_000;

/// 净值 REST 轮询间隔 (毫秒)。一次 = balance + positions 各一次，两者官方限速各 10/2s
const EQUITY_POLL_INTERVAL_MS: u64 = 2_000;

/// OkxNode 构造参数
pub struct OkxArgs {
    /// 账户接口（私有 WS 登录、净值与资金费轮询用；与 ExchangeSetup 里的是同一个）。
    /// 只在配了凭证时有，没有就只接公共行情
    pub account: Option<Arc<OkxAccount>>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_bus: MarketBus,
    /// 账户私有事件总线（private WS / 账户轮询用）
    pub account_bus: AccountBus,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// OKX 交易所节点
pub struct OkxNode {
    inbox: Inbox<ExchangeCmd>,
    /// 公共 WebSocket（除 K 线外的全部行情），第一条订阅时才连
    public_ws: LazyConn,
    /// Business WebSocket（K 线），第一条 K 线订阅时才连
    business_ws: LazyConn,
}

/// 按订阅 kind 选择落到哪条 WS：K 线只在 business 线上有
fn is_business(kind: &SubscriptionKind) -> bool {
    matches!(kind, SubscriptionKind::Candle { .. })
}

impl OkxNode {
    /// 在 `group` 下起 OKX 节点，等私有 WS 连好（含登录与订阅确认），返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试）。
    pub async fn spawn(group: &Group, args: OkxArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("OkxNode", move |scope: Scope, inbox| async move {
                let OkxArgs {
                    account,
                    symbol_metas,
                    market_bus,
                    account_bus,
                    quote,
                } = args;
                let children = scope.group("children");

                // 1. 私有 WS 启动时就连（含登录与订阅确认，要在投产前生效）；失败 → 向上传播。
                //    两条公共 WS 等第一条订阅路由过来才连（见 LazyConn）
                let has_private_ws = account.is_some();
                if let Some(account) = &account {
                    OkxPrivateWs::spawn(
                        &children,
                        account.credentials().clone(),
                        account_bus.clone(),
                        symbol_metas.clone(),
                        quote.clone(),
                    )
                    .await?;
                }
                let public_ws = {
                    let (group, market_bus, metas, quote) = (
                        children.clone(),
                        market_bus.clone(),
                        symbol_metas.clone(),
                        quote.clone(),
                    );
                    LazyConn::new("OkxPublicWs", move || async move {
                        OkxPublicWs::spawn(&group, market_bus, metas, quote).await
                    })
                };
                let business_ws = {
                    let (group, market_bus, quote) =
                        (children.clone(), market_bus.clone(), quote.clone());
                    LazyConn::new("OkxBusinessWs", move || async move {
                        OkxBusinessWs::spawn(&group, market_bus, quote).await
                    })
                };

                // 2. 账户轮询与私有 WS 一样只在有账户时起
                if let Some(account) = account {
                    // 净值走 REST 轮询（计价币那一项的 eq，见 OkxAccount::fetch_equity）
                    EquityPoller::spawn(
                        &children,
                        account.clone(),
                        account_bus.clone(),
                        EQUITY_POLL_INTERVAL_MS,
                    )?;
                    FundingFeePoller::spawn(
                        &children,
                        account,
                        account_bus.clone(),
                        FUNDING_FEE_POLL_INTERVAL_MS,
                    )?;
                }
                CryptoStatus::spawn(
                    &children,
                    crate::domain::Exchange::OKX,
                    market_bus.clone(),
                    STATUS_BROADCAST_INTERVAL_MS,
                )?;

                tracing::info!(exchange = "OKX", has_private_ws, "OkxNode started");
                Ok(Self {
                    inbox,
                    public_ws,
                    business_ws,
                })
            })
            .await?;
        Ok(ExchangeHandle::new(addr))
    }

    /// 按目标 WS 路由命令；批量订阅按目标拆分
    async fn route(&mut self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        match cmd {
            ExchangeCmd::SubscribeBatch(kinds) => {
                let (business_kinds, public_kinds): (Vec<_>, Vec<_>) =
                    kinds.into_iter().partition(is_business);
                if !public_kinds.is_empty() {
                    self.public_ws
                        .send(ExchangeCmd::SubscribeBatch(public_kinds))
                        .await?;
                }
                if !business_kinds.is_empty() {
                    self.business_ws
                        .send(ExchangeCmd::SubscribeBatch(business_kinds))
                        .await?;
                }
                Ok(())
            }
        }
    }
}

impl Task for OkxNode {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 子 WS 只在出错时提前停止收件（出错会自己上冒），转发失败就整支停
        while let Some(cmd) = self.inbox.recv().await {
            self.route(cmd).await?;
        }
        Ok(())
    }
}

//! HyperliquidNode - Hyperliquid 交易所节点
//!
//! 职责:
//! - 管理公共/私有 WS 和各轮询子任务
//! - 把订阅命令转发到公共 WS
//! - WS 组件直接解析消息并发布到对应总线
//!
//! 一个节点覆盖 client 接入的全部 perp dex（共用一个钱包、一条公共 WS、一条私有 WS），
//! 品种归属由 client 的品种目录决定。
//!
//! 树:
//! HyperliquidNode
//! ├── HyperliquidPublicWs（第一条订阅时才连）
//! ├── HyperliquidPrivateWs（需要凭证）
//! ├── EquityPoller（需要凭证）
//! ├── FundingFeePoller（需要凭证）
//! └── CryptoStatus

use super::private_ws::HyperliquidPrivateWs;
use super::public_ws::HyperliquidPublicWs;
use crate::exchange::client::{ExchangeCmd, ExchangeHandle};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::funding_fee_polling::{FundingFeePoller, FUNDING_FEE_POLL_INTERVAL_MS};
use crate::exchange::hyperliquid::{HyperliquidAccount, HyperliquidClient};
use crate::exchange::lazy_conn::LazyConn;
use crate::exchange::CryptoStatus;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Group, Inbox, Scope, Task};
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 5_000;

/// 账户净值轮询间隔 (毫秒)。一次 = spotClearinghouseState + 每个已接入 dex 一次
/// clearinghouseState（各约 2 权重）；2 秒一次、两个 dex 约 180 权重/分钟，
/// 与持仓对账、资金费拉取合计仍远低于 HL 每 IP 1200/分钟。陈旧守卫是 60 秒，间隔不影响它。
const EQUITY_POLL_INTERVAL_MS: u64 = 2_000;

/// HyperliquidNode 构造参数
pub struct HyperliquidArgs {
    /// 公共接口（品种目录在里面；与 ExchangeSetup 里的是同一个）
    pub client: Arc<HyperliquidClient>,
    /// 账户接口（私有 WS 的查询地址、净值与资金费轮询用）。只在配了凭证时有
    pub account: Option<Arc<HyperliquidAccount>>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_bus: MarketBus,
    /// 账户私有事件总线（private WS / 账户轮询用）
    pub account_bus: AccountBus,
}

/// Hyperliquid 交易所节点
pub struct HyperliquidNode {
    inbox: Inbox<ExchangeCmd>,
    /// 公共 WebSocket（全部订阅命令都转给它），第一条订阅时才连
    public_ws: LazyConn,
}

impl HyperliquidNode {
    /// 在 `group` 下起 Hyperliquid 节点，等私有 WS 连好（含订阅确认），返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试/不重连）。
    pub async fn spawn(group: &Group, args: HyperliquidArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("HyperliquidNode", move |scope: Scope, inbox| async move {
                let client = args.client;
                let markets = client.markets().clone();
                let has_private_ws = args.account.is_some();
                let children = scope.group("children");

                // 1. 私有 WS 启动时就连（含订阅确认，要在投产前生效）；失败 → 向上传播。
                //    公共 WS 等第一条订阅路由过来才连（见 LazyConn）
                if let Some(account) = &args.account {
                    HyperliquidPrivateWs::spawn(
                        &children,
                        account.wallet_address().to_string(),
                        args.account_bus.clone(),
                        markets.clone(),
                    )
                    .await?;
                }
                let public_ws = {
                    let (group, market_bus, markets) = (children.clone(), args.market_bus.clone(), markets.clone());
                    LazyConn::new("HyperliquidPublicWs", move || async move {
                        HyperliquidPublicWs::spawn(&group, market_bus, markets).await
                    })
                };
                tracing::info!(exchange = "Hyperliquid", has_private_ws, dexes = ?markets.dexes(), "私有 WS ready");

                // 2. 账户轮询（净值、资费结算）：与私有 WS 一样按凭证门控
                if let Some(account) = args.account {
                    EquityPoller::spawn(&children, account.clone(), args.account_bus.clone(), EQUITY_POLL_INTERVAL_MS)?;
                    FundingFeePoller::spawn(
                        &children,
                        account,
                        args.account_bus.clone(),
                        FUNDING_FEE_POLL_INTERVAL_MS,
                    )?;
                }

                // 3. 市场状态广播（加密货币 7x24 始终 Liquid）
                CryptoStatus::spawn(
                    &children,
                    crate::domain::Exchange::Hyperliquid,
                    args.market_bus.clone(),
                    STATUS_BROADCAST_INTERVAL_MS,
                )?;

                tracing::info!(exchange = "Hyperliquid", has_private_ws, "HyperliquidNode started");
                Ok(Self { inbox, public_ws })
            })
            .await?;
        Ok(ExchangeHandle::new(addr))
    }
}

impl Task for HyperliquidNode {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 子 WS 只在出错时提前停止收件（出错会自己上冒），转发失败就整支停
        while let Some(cmd) = self.inbox.recv().await {
            self.public_ws.send(cmd).await?;
        }
        Ok(())
    }
}

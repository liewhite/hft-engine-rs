//! HyperliquidNode - Hyperliquid 交易所节点
//!
//! 职责:
//! - 管理公共/私有 WS 和各轮询子任务
//! - 把订阅/退订命令转发到公共 WS
//! - WS 组件直接解析消息并发布到对应总线
//!
//! 一个节点覆盖 client 接入的全部 perp dex（共用一个钱包、一条公共 WS、一条私有 WS），
//! 品种归属由 client 的品种目录决定。
//!
//! 树:
//! HyperliquidNode
//! ├── HyperliquidPublicWs
//! ├── HyperliquidPrivateWs（需要凭证）
//! ├── EquityPoller（需要凭证）
//! ├── FundingFeePoller（需要凭证）
//! └── CryptoStatus

use super::private_ws::HyperliquidPrivateWs;
use super::public_ws::HyperliquidPublicWs;
use crate::exchange::client::{ExchangeCmd, ExchangeHandle};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::funding_fee_polling::{FundingFeePoller, FUNDING_FEE_POLL_INTERVAL_MS};
use crate::exchange::hyperliquid::HyperliquidClient;
use crate::exchange::CryptoStatus;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 5_000;

/// 账户净值轮询间隔 (毫秒)。一次 = spotClearinghouseState + 每个已接入 dex 一次
/// clearinghouseState（各约 2 权重）；2 秒一次、两个 dex 约 180 权重/分钟，
/// 与持仓对账、资金费拉取合计仍远低于 HL 每 IP 1200/分钟。陈旧守卫是 60 秒，间隔不影响它。
const EQUITY_POLL_INTERVAL_MS: u64 = 2_000;

/// HyperliquidNode 构造参数
pub struct HyperliquidArgs {
    /// 装配处建好的 client（品种目录、凭证都在里面；与 ExchangeSetup 里的是同一个）
    pub client: Arc<HyperliquidClient>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_bus: MarketBus,
    /// 账户私有事件总线（private WS / 账户轮询用）
    pub account_bus: AccountBus,
}

/// Hyperliquid 交易所节点
pub struct HyperliquidNode {
    inbox: Inbox<ExchangeCmd>,
    /// 公共 WebSocket（全部订阅命令都转给它）
    public_ws: Addr<ExchangeCmd>,
}

impl HyperliquidNode {
    /// 在 `group` 下起 Hyperliquid 节点，等全部 WS 连好（私有的含订阅确认），返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试/不重连）。
    pub async fn spawn(group: &Group, args: HyperliquidArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("HyperliquidNode", move |scope: Scope, inbox| async move {
                let client = args.client;
                let markets = client.markets().clone();
                // 有凭证 = client 有账户（同一个 Option 的两面），私有 WS 与各轮询都按它门控
                let private_feed = client.private_feed();
                let has_private_ws = private_feed.is_some();
                let children = scope.group("children");

                // 1. 两条 WS 并发连接；任一失败 → 向上传播
                let private = async {
                    match private_feed.clone() {
                        Some(wallet_address) => HyperliquidPrivateWs::spawn(
                            &children,
                            wallet_address,
                            args.account_bus.clone(),
                            markets.clone(),
                        )
                        .await
                        .map(|_| ()),
                        None => Ok(()),
                    }
                };
                let (public_ws, ()) = tokio::try_join!(
                    HyperliquidPublicWs::spawn(&children, args.market_bus.clone(), markets.clone()),
                    private,
                )?;
                tracing::info!(exchange = "Hyperliquid", has_private_ws, dexes = ?markets.dexes(), "WS ready");

                // 2. 账户轮询（净值、资费结算）：与私有 WS 一样按凭证门控
                if has_private_ws {
                    EquityPoller::spawn(&children, client.clone(), args.account_bus.clone(), EQUITY_POLL_INTERVAL_MS)?;
                    FundingFeePoller::spawn(
                        &children,
                        client,
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
            self.public_ws
                .send(cmd)
                .context("转发订阅命令失败：Hyperliquid 公共 WS 已停止收件")?;
        }
        Ok(())
    }
}

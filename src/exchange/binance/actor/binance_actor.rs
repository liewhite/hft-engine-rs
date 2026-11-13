//! BinanceNode - Binance 交易所节点
//!
//! 职责:
//! - 管理公共/私有 WS 和各轮询子任务
//! - 把订阅/退订命令按 kind 路由到对应的公共 WS
//! - WS 组件直接解析消息并发布到对应总线
//!
//! 树:
//! BinanceNode
//! ├── BinancePublicWs（/public/ws，第一条订阅时才连）
//! ├── BinancePublicWs（/market/ws，第一条订阅时才连）
//! ├── BinancePrivateWs（需要凭证）
//! │   └── ListenKeyRefresher
//! ├── EquityPoller（需要凭证）
//! ├── FundingFeePoller（需要凭证）
//! └── CryptoStatus

use super::private_ws::BinancePrivateWs;
use super::public_ws::BinancePublicWs;
use crate::exchange::binance::{BinanceAccount, WS_MARKET_URL, WS_PUBLIC_HIGH_FREQ_URL};
use crate::exchange::client::{ExchangeCmd, ExchangeHandle, SubscriptionKind};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::funding_fee_polling::{FundingFeePoller, FUNDING_FEE_POLL_INTERVAL_MS};
use crate::exchange::lazy_conn::LazyConn;
use crate::exchange::CryptoStatus;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Group, Inbox, Scope, Task};
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 60_000;

/// BinanceNode 构造参数
pub struct BinanceArgs {
    /// 账户接口（私有流的 ListenKey、净值与资金费轮询用；与 ExchangeSetup 里的是同一个）。
    /// 只在配了凭证时有，没有就只接公共行情
    pub account: Option<Arc<BinanceAccount>>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_bus: MarketBus,
    /// 账户私有事件总线（private WS / 账户轮询用）
    pub account_bus: AccountBus,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// Binance 交易所节点
pub struct BinanceNode {
    inbox: Inbox<ExchangeCmd>,
    /// 盘口高频 WebSocket（/public/ws：bookTicker、depth），第一条订阅时才连
    public_ws: LazyConn,
    /// 常规市场 WebSocket（/market/ws：markPrice、kline、ticker、aggTrade），第一条订阅时才连
    market_ws: LazyConn,
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

impl BinanceNode {
    /// 在 `group` 下起 Binance 节点，等私有 WS 连好（含 ListenKey 就位），返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试），避免"假就绪"窗口。
    pub async fn spawn(group: &Group, args: BinanceArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("BinanceNode", move |scope: Scope, inbox| async move {
                let BinanceArgs {
                    account,
                    market_bus,
                    account_bus,
                    quote,
                } = args;
                let children = scope.group("children");

                // 1. 私有 WS 启动时就连（要在投产前确认订阅生效）；失败 → 向上传播。
                //    两条公共 WS 等第一条订阅路由过来才连（见 LazyConn）
                let has_private_ws = account.is_some();
                if let Some(account) = &account {
                    BinancePrivateWs::spawn(
                        &children,
                        account.clone(),
                        account_bus.clone(),
                        quote.clone(),
                    )
                    .await?;
                }
                let lazy_public = |name: &'static str, target: WsTarget| {
                    let (group, market_bus, quote) =
                        (children.clone(), market_bus.clone(), quote.clone());
                    LazyConn::new(name, move || async move {
                        BinancePublicWs::spawn(&group, name, target.url(), market_bus, quote).await
                    })
                };
                let public_ws = lazy_public("BinancePublicWs", WsTarget::PublicHighFreq);
                let market_ws = lazy_public("BinanceMarketWs", WsTarget::Market);
                // 2. 私有轮询与私有 WS 一样只在有账户时起
                if let Some(account) = account {
                    EquityPoller::spawn(&children, account.clone(), account_bus.clone(), 1000)?;
                    FundingFeePoller::spawn(
                        &children,
                        account,
                        account_bus.clone(),
                        FUNDING_FEE_POLL_INTERVAL_MS,
                    )?;
                }
                CryptoStatus::spawn(
                    &children,
                    crate::domain::Exchange::Binance,
                    market_bus.clone(),
                    STATUS_BROADCAST_INTERVAL_MS,
                )?;

                tracing::info!(exchange = "Binance", has_private_ws, "BinanceNode started");
                Ok(Self {
                    inbox,
                    public_ws,
                    market_ws,
                })
            })
            .await?;
        Ok(ExchangeHandle::new(addr))
    }

    fn ws_for(&mut self, target: WsTarget) -> &mut LazyConn {
        match target {
            WsTarget::PublicHighFreq => &mut self.public_ws,
            WsTarget::Market => &mut self.market_ws,
        }
    }

    /// 按目标 WS 路由命令。批量订阅按目标拆分，避免把 markPrice 流发到 /public/ws 被拒
    async fn route(&mut self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        match cmd {
            ExchangeCmd::Subscribe(ref kind) | ExchangeCmd::Unsubscribe(ref kind) => {
                let target = pick_ws_target(kind);
                self.ws_for(target).send(cmd).await
            }
            ExchangeCmd::SubscribeBatch(kinds) => {
                let (public_kinds, market_kinds): (Vec<_>, Vec<_>) = kinds
                    .into_iter()
                    .partition(|k| pick_ws_target(k) == WsTarget::PublicHighFreq);
                for (target, kinds) in [
                    (WsTarget::PublicHighFreq, public_kinds),
                    (WsTarget::Market, market_kinds),
                ] {
                    if !kinds.is_empty() {
                        self.ws_for(target)
                            .send(ExchangeCmd::SubscribeBatch(kinds))
                            .await?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl Task for BinanceNode {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 子 WS 只在出错时提前停止收件（出错会自己上冒），转发失败就整支停
        while let Some(cmd) = self.inbox.recv().await {
            self.route(cmd).await?;
        }
        Ok(())
    }
}

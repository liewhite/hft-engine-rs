//! BinanceNode - Binance 交易所节点
//!
//! 职责:
//! - 管理公共/私有 WS 和各轮询子任务
//! - 把订阅/退订命令按 kind 路由到对应的公共 WS
//! - WS 组件直接解析消息并发布到对应总线
//!
//! 树:
//! BinanceNode
//! ├── BinancePublicWs（/public/ws）
//! ├── BinancePublicWs（/market/ws）
//! ├── BinancePrivateWs（需要凭证）
//! │   └── ListenKeyRefresher
//! ├── EquityPoller（需要凭证）
//! ├── FundingFeePoller（需要凭证）
//! └── CryptoStatus

use super::private_ws::BinancePrivateWs;
use super::public_ws::BinancePublicWs;
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::binance::{
    BinanceClient, BinanceCredentials, WS_MARKET_URL, WS_PUBLIC_HIGH_FREQ_URL,
};
use crate::exchange::client::{ExchangeCmd, ExchangeHandle, SubscriptionKind};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::funding_fee_polling::{FundingFeePoller, FUNDING_FEE_POLL_INTERVAL_MS};
use crate::exchange::CryptoStatus;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use std::collections::HashMap;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 60_000;

/// BinanceNode 构造参数
pub struct BinanceArgs {
    /// 凭证（可选）
    pub credentials: Option<BinanceCredentials>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// REST 基础 URL（用于 ListenKey）
    pub rest_base_url: String,
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
    /// 盘口高频 WebSocket（/public/ws：bookTicker、depth）
    public_ws: Addr<ExchangeCmd>,
    /// 常规市场 WebSocket（/market/ws：markPrice、kline、ticker、aggTrade）
    market_ws: Addr<ExchangeCmd>,
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
    /// 在 `group` 下起 Binance 节点，等全部 WS 连好（私有的含 ListenKey 就位），返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试），避免"假就绪"窗口。
    pub async fn spawn(group: &Group, args: BinanceArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("BinanceNode", move |scope: Scope, inbox| async move {
                // 拆开：下面的 async 块按值取走凭证，其余字段在同一个 try_join! 里被借用
                let BinanceArgs { credentials, symbol_metas, rest_base_url, market_bus, account_bus, quote } = args;
                // 凭证可用时构建 Arc<BinanceClient>，供 FundingFee / equity 轮询使用。
                // 初始持仓查询在 manager 投产编排里（executor 注册后），不在此处。
                // 构建失败向上传播 → 启动期受控退出
                let binance_client: Option<Arc<BinanceClient>> = match credentials.as_ref() {
                    Some(c) => Some(Arc::new(
                        BinanceClient::new(quote.clone(), Some(c.clone()))
                            .context("Failed to create BinanceClient")?,
                    )),
                    None => None,
                };
                let children = scope.group("children");

                // 1. 三条 WS 并发连接；任一失败 → 向上传播
                let has_private_ws = credentials.is_some();
                let (private_group, private_account_bus, private_quote) =
                    (children.clone(), account_bus.clone(), quote.clone());
                let private = async move {
                    match credentials {
                        Some(credentials) => BinancePrivateWs::spawn(
                            &private_group,
                            credentials,
                            rest_base_url,
                            private_account_bus,
                            private_quote,
                        )
                        .await
                        .map(|_| ()),
                        None => Ok(()),
                    }
                };
                let (public_ws, market_ws, ()) = tokio::try_join!(
                    BinancePublicWs::spawn(
                        &children,
                        "BinancePublicWs",
                        WsTarget::PublicHighFreq.url(),
                        market_bus.clone(),
                        symbol_metas.clone(),
                        quote.clone(),
                    ),
                    BinancePublicWs::spawn(
                        &children,
                        "BinanceMarketWs",
                        WsTarget::Market.url(),
                        market_bus.clone(),
                        symbol_metas.clone(),
                        quote.clone(),
                    ),
                    private,
                )?;
                tracing::info!(exchange = "Binance", has_private_ws, "WS ready");

                // 2. 私有轮询与私有 WS 一样按凭证门控：无凭证时只订阅公共行情（non-auth 模式），
                // 否则 equity 轮询会每秒失败一次、刷满日志（既是噪音，也会掩盖真实告警）。
                // 用同一个带凭证的 client（`binance_client` 与 `has_private_ws` 同源于凭证是否存在）：
                // 净值查询是账户私有能力，只有 AccountClient 提供
                if let Some(client) = binance_client {
                    EquityPoller::spawn(&children, client.clone(), account_bus.clone(), 1000)?;
                    FundingFeePoller::spawn(&children, client, account_bus.clone(), FUNDING_FEE_POLL_INTERVAL_MS)?;
                }
                CryptoStatus::spawn(
                    &children,
                    crate::domain::Exchange::Binance,
                    market_bus.clone(),
                    STATUS_BROADCAST_INTERVAL_MS,
                )?;

                tracing::info!(exchange = "Binance", has_private_ws, "BinanceNode started");
                Ok(Self { inbox, public_ws, market_ws })
            })
            .await?;
        Ok(ExchangeHandle::new(addr))
    }

    fn ws_for(&self, target: WsTarget) -> &Addr<ExchangeCmd> {
        match target {
            WsTarget::PublicHighFreq => &self.public_ws,
            WsTarget::Market => &self.market_ws,
        }
    }

    /// 按目标 WS 路由命令。批量订阅按目标拆分，避免把 markPrice 流发到 /public/ws 被拒
    fn route(&self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        match cmd {
            ExchangeCmd::Subscribe(ref kind) | ExchangeCmd::Unsubscribe(ref kind) => {
                let target = self.ws_for(pick_ws_target(kind));
                target.send(cmd).context("转发订阅命令失败：公共 WS 已停止收件")?;
            }
            ExchangeCmd::SubscribeBatch(kinds) => {
                let (public_kinds, market_kinds): (Vec<_>, Vec<_>) = kinds
                    .into_iter()
                    .partition(|k| pick_ws_target(k) == WsTarget::PublicHighFreq);
                for (target, kinds) in [(&self.public_ws, public_kinds), (&self.market_ws, market_kinds)] {
                    if !kinds.is_empty() {
                        target
                            .send(ExchangeCmd::SubscribeBatch(kinds))
                            .context("转发批量订阅失败：公共 WS 已停止收件")?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Task for BinanceNode {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 子 WS 只在出错时提前停止收件（出错会自己上冒），转发失败就整支停
        while let Some(cmd) = self.inbox.recv().await {
            self.route(cmd)?;
        }
        Ok(())
    }
}

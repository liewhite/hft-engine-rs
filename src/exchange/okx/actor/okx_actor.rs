//! OkxNode - OKX 交易所节点
//!
//! 职责:
//! - 管理公共/私有/业务 WS 和各轮询子任务
//! - 把订阅/退订命令按 kind 路由：Candle 到 Business WS，其余到公共 WS
//! - WS 组件直接解析消息并发布到对应总线
//!
//! 树:
//! OkxNode
//! ├── OkxPublicWs
//! ├── OkxBusinessWs（K线）
//! ├── OkxPrivateWs（需要凭证）
//! ├── EquityPoller（需要凭证；净值 / 持仓总价值）
//! ├── OkxGreeksPoller（需要凭证）
//! ├── FundingFeePoller（需要凭证）
//! └── CryptoStatus

use super::business_ws::OkxBusinessWs;
use super::greeks_polling::OkxGreeksPoller;
use super::private_ws::OkxPrivateWs;
use super::public_ws::OkxPublicWs;
use crate::domain::{Symbol, SymbolMeta};
use crate::exchange::client::{ExchangeCmd, ExchangeHandle, SubscriptionKind};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::funding_fee_polling::{FundingFeePoller, FUNDING_FEE_POLL_INTERVAL_MS};
use crate::exchange::okx::{OkxClient, OkxCredentials};
use crate::exchange::CryptoStatus;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use std::collections::HashMap;
use std::sync::Arc;

/// 市场状态广播间隔 (毫秒)
const STATUS_BROADCAST_INTERVAL_MS: u64 = 60_000;

/// 净值 REST 轮询间隔 (毫秒)。一次 = balance + positions 各一次，两者官方限速各 10/2s
const EQUITY_POLL_INTERVAL_MS: u64 = 2_000;

/// Greeks REST 轮询间隔 (毫秒) — 每秒 1 次，官方限速 10/2s
const GREEKS_POLLING_INTERVAL_MS: u64 = 1000;

/// OkxNode 构造参数
pub struct OkxArgs {
    /// 凭证（可选）
    pub credentials: Option<OkxCredentials>,
    /// REST client (用于净值 / Greeks 轮询)
    pub client: Option<Arc<OkxClient>>,
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
    /// 公共 WebSocket（除 K 线外的全部行情）
    public_ws: Addr<ExchangeCmd>,
    /// Business WebSocket（K 线）
    business_ws: Addr<ExchangeCmd>,
}

/// 按订阅 kind 选择落到哪条 WS：K 线只在 business 线上有
fn is_business(kind: &SubscriptionKind) -> bool {
    matches!(kind, SubscriptionKind::Candle { .. })
}

impl OkxNode {
    /// 在 `group` 下起 OKX 节点，等全部 WS 连好（私有的含登录与订阅确认），返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试）。
    pub async fn spawn(group: &Group, args: OkxArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("OkxNode", move |scope: Scope, inbox| async move {
                let OkxArgs { credentials, client, symbol_metas, market_bus, account_bus, quote } = args;
                let children = scope.group("children");

                // 1. 三条 WS 并发连接；任一失败 → 向上传播（启动期受控退出，不重试）
                let has_private_ws = credentials.is_some();
                let private = async {
                    match credentials {
                        Some(credentials) => OkxPrivateWs::spawn(
                            &children,
                            credentials,
                            account_bus.clone(),
                            symbol_metas.clone(),
                            quote.clone(),
                        )
                        .await
                        .map(|_| ()),
                        None => Ok(()),
                    }
                };
                let (public_ws, business_ws, ()) = tokio::try_join!(
                    OkxPublicWs::spawn(&children, market_bus.clone(), symbol_metas.clone(), quote.clone()),
                    OkxBusinessWs::spawn(&children, market_bus.clone(), symbol_metas.clone(), quote.clone()),
                    private,
                )?;
                tracing::info!(exchange = "OKX", has_private_ws, "WS ready");

                // 2. Greeks 轮询要签名，故按**凭证**门控：装配处只在有凭证时才给 client
                // （见 assembly::setup_okx），所以这里 `Some` 即"可轮询"，不必再判一次 bool。
                // 无凭证时不起它 —— 否则 non-auth 模式会每秒失败刷日志。
                if let Some(client) = client {
                    // 净值与持仓总价值走 REST 轮询：私有 WS 推送的 notionalUsd 在单币种保证金模式下是空串
                    EquityPoller::spawn(&children, client.clone(), account_bus.clone(), EQUITY_POLL_INTERVAL_MS)?;
                    OkxGreeksPoller::spawn(&children, client.clone(), account_bus.clone(), GREEKS_POLLING_INTERVAL_MS)?;
                    FundingFeePoller::spawn(&children, client, account_bus.clone(), FUNDING_FEE_POLL_INTERVAL_MS)?;
                }
                CryptoStatus::spawn(
                    &children,
                    crate::domain::Exchange::OKX,
                    market_bus.clone(),
                    STATUS_BROADCAST_INTERVAL_MS,
                )?;

                tracing::info!(exchange = "OKX", has_private_ws, "OkxNode started");
                Ok(Self { inbox, public_ws, business_ws })
            })
            .await?;
        Ok(ExchangeHandle::new(addr))
    }

    fn ws_for(&self, kind: &SubscriptionKind) -> &Addr<ExchangeCmd> {
        if is_business(kind) {
            &self.business_ws
        } else {
            &self.public_ws
        }
    }

    /// 按目标 WS 路由命令；批量订阅按目标拆分
    fn route(&self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        match cmd {
            ExchangeCmd::Subscribe(ref kind) | ExchangeCmd::Unsubscribe(ref kind) => {
                self.ws_for(kind)
                    .send(cmd)
                    .context("转发订阅命令失败：OKX WS 已停止收件")?;
            }
            ExchangeCmd::SubscribeBatch(kinds) => {
                let (business_kinds, public_kinds): (Vec<_>, Vec<_>) =
                    kinds.into_iter().partition(is_business);
                for (target, kinds) in [(&self.public_ws, public_kinds), (&self.business_ws, business_kinds)] {
                    if !kinds.is_empty() {
                        target
                            .send(ExchangeCmd::SubscribeBatch(kinds))
                            .context("转发批量订阅失败：OKX WS 已停止收件")?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl Task for OkxNode {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 子 WS 只在出错时提前停止收件（出错会自己上冒），转发失败就整支停
        while let Some(cmd) = self.inbox.recv().await {
            self.route(cmd)?;
        }
        Ok(())
    }
}

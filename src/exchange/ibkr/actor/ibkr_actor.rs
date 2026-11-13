//! IbkrNode - IBKR 交易所节点
//!
//! 职责:
//! - 管理子任务（WS、tickle、各轮询）
//! - 转发订阅/退订命令到 IbkrPublicWs
//!
//! 树:
//! IbkrNode
//! ├── IbkrPublicWs（行情 smd + 订单 sor + 成交 str，同一条 WS）
//! │   └── 读循环（WsConn）
//! ├── IbkrTickle
//! ├── EquityPoller（通用，见 crate::exchange::equity_polling）
//! ├── IbkrStatusPoller
//! └── IbkrSnapshotPoller（可选）

use super::public_ws::{IbkrPublicWs, IbkrPublicWsArgs};
use super::snapshot_polling::{IbkrSnapshotConfig, IbkrSnapshotPoller, MD_AVAILABILITY_FIELD};
use super::status_polling::IbkrStatusPoller;
use super::tickle::IbkrTickle;
use crate::domain::{Exchange, ExchangeError};
use crate::exchange::client::{ExchangeCmd, ExchangeHandle};
use crate::exchange::equity_polling::EquityPoller;
use crate::exchange::ibkr::auth::{self, IbkrAuth};
use crate::exchange::ibkr::IbkrClient;
use crate::messaging::{AccountBus, MarketBus};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use std::collections::HashMap;
use std::sync::Arc;

/// IBKR 账户信息轮询间隔 (毫秒)
const ACCOUNT_POLLING_INTERVAL_MS: u64 = 3000;

/// 市场状态轮询间隔 (毫秒)
const STATUS_POLLING_INTERVAL_MS: u64 = 5_000;

/// IbkrNode 构造参数
pub struct IbkrArgs {
    /// 认证器 (共享，不可变)
    pub auth: Arc<dyn IbkrAuth>,
    /// 公共行情总线（public WS / 状态广播用）
    pub market_bus: MarketBus,
    /// 账户私有事件总线（WS 上的订单/成交推送、账户轮询用）
    pub account_bus: AccountBus,
    /// conid 映射 (symbol → conid)
    pub conids: HashMap<String, i64>,
    /// IBKR 客户端 (用于账户/状态/snapshot 轮询与行情预热)
    pub client: Arc<IbkrClient>,
    /// 借券费/汇率 snapshot 轮询配置；Some 时起一个 IbkrSnapshotPoller。
    /// None = 不轮询（无兜底常量，上层相关读数保持缺失）。
    pub snapshot: Option<IbkrSnapshotConfig>,
}

/// IBKR 交易所节点
pub struct IbkrNode {
    inbox: Inbox<ExchangeCmd>,
    public_ws: Addr<ExchangeCmd>,
}

impl IbkrNode {
    /// 在 `group` 下起 IBKR 节点，等 WS 握手完成、各子任务起好，返回句柄。
    /// 任一失败 → 返回 Err 并上冒（启动期受控退出，不重试）。
    pub async fn spawn(group: &Group, args: IbkrArgs) -> anyhow::Result<ExchangeHandle> {
        let (_, addr) = group
            .spawn_addressed("IbkrNode", move |scope: Scope, inbox| async move {
                // 1. Tickle 获取 session_id (供 WS Cookie 使用)
                let http = args
                    .auth
                    .build_http_client()
                    .context("IBKR failed to build HTTP client")?;
                let session_id = auth::tickle(&*args.auth, &http).await.map_err(|e| {
                    // 保留原始上下文（网络/非 2xx/解析/缺 session 等）便于诊断，
                    // tickle 内部错误信息不含 token/cookie，安全。
                    tracing::error!(exchange = "IBKR", error = %e, "Initial tickle failed");
                    ExchangeError::new(format!("{} 鉴权失败", Exchange::IBKR))
                })?;

                let children = scope.group("children");

                // 2. 起 PublicWs（等握手完成）与 Tickle。
                //    IBKR 用同一条 WS 同时收行情和订单更新，必须握手完成再放行下游。
                // IBKR 每个 conid 只维护一份活跃字段集（snapshot 与 ws 共用），而重建它的唯一地方是
                // public_ws 的 smd 订阅/刷新。故把 snapshot poller 需要的字段一并交给它，否则每次刷新
                // 都会把 poller 的字段抹掉、poller 只能拿到空壳（实测每 8~12 分钟一条券源字段缺失错误）。
                let extra_md_fields = args
                    .snapshot
                    .as_ref()
                    .map(|c| {
                        vec![
                            c.shortable_field.clone(),
                            c.fee_field.clone(),
                            MD_AVAILABILITY_FIELD.to_string(),
                        ]
                    })
                    .unwrap_or_default();
                let public_ws = IbkrPublicWs::spawn(
                    &children,
                    IbkrPublicWsArgs {
                        auth: args.auth.clone(),
                        client: args.client.clone(),
                        market_bus: args.market_bus.clone(),
                        account_bus: args.account_bus.clone(),
                        conids: args.conids,
                        session_id,
                        extra_md_fields,
                    },
                )
                .await?;
                IbkrTickle::spawn(&children, args.auth.clone(), http)?;
                tracing::info!(exchange = "IBKR", "WS + tickle ready");

                // 3. 账户净值轮询 (每 3 秒)：IBKR WebSocket 不推送账户级 equity/notional。
                //    持仓不在此处刷新——初始持仓由 Manager 启动期统一 fetch，运行期靠 Fill 维护
                EquityPoller::spawn(
                    &children,
                    args.client.clone(),
                    args.account_bus.clone(),
                    ACCOUNT_POLLING_INTERVAL_MS,
                )?;

                // 4. 市场状态轮询
                IbkrStatusPoller::spawn(
                    &children,
                    args.client.clone(),
                    args.market_bus.clone(),
                    STATUS_POLLING_INTERVAL_MS,
                )?;

                // 5. 借券费/汇率 snapshot 轮询（可选）：它一旦致命退出（数据源失效）经任务树上冒、整机重启
                if let Some(cfg) = args.snapshot {
                    IbkrSnapshotPoller::spawn(
                        &children,
                        args.client,
                        args.market_bus.clone(),
                        cfg,
                    )?;
                }

                tracing::info!(exchange = "IBKR", "IbkrNode started");
                Ok(Self { inbox, public_ws })
            })
            .await?;
        Ok(ExchangeHandle::new(addr))
    }
}

impl Task for IbkrNode {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // 子 WS 只在出错时提前停止收件（出错会自己上冒），转发失败就整支停
        while let Some(cmd) = self.inbox.recv().await {
            self.public_ws
                .send(cmd)
                .context("转发订阅命令失败：IbkrPublicWs 已停止收件")?;
        }
        Ok(())
    }
}

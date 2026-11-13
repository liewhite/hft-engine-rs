pub(crate) mod actor;
pub mod auth;
pub(crate) mod client;
mod gateway;
pub mod oauth;
mod symbol;
pub(crate) mod wire;

pub use actor::{IbkrArgs, IbkrNode, IbkrSnapshotConfig};
pub use auth::IbkrAuth;
pub use client::IbkrClient;

use gateway::IbkrGateway;
use oauth::IbkrOAuth;
use std::sync::Arc;

/// 本引擎在 IBKR 的订单号（cOID / order_ref）：`ib` + 30 hex
pub const CLIENT_ORDER_IDS: super::CliOrderIdFormat = super::CliOrderIdFormat::new("ib", 30);

/// IBKR OAuth REST API 基础 URL
pub const OAUTH_BASE_URL: &str = "https://api.ibkr.com/v1/api/";

/// IBKR REST 单个请求的上限（网关 / OAuth 两种接入共用）
pub(crate) const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// WS 保活：会话由 tickle 保活（REST），WS 侧只做空闲检测；smd 有 8 分钟重订阅周期，
/// 空闲上限给足余量
pub(crate) const KEEPALIVE: crate::exchange::ws_loop::WsKeepalive =
    crate::exchange::ws_loop::WsKeepalive {
        ping_text: None,
        ping_interval: std::time::Duration::from_secs(60),
        idle_timeout: std::time::Duration::from_secs(12 * 60),
    };

// ---------------------------------------------------------------------------
// 限频（#791，机制见 crate::exchange::rate_limit）。数字取自官方 Web API「Pacing Limitations」
// （2026-11 实查）：全局 10 次 / 秒（经 CP Gateway；OAuth 是 50，按严的算），部分接口另有更严的限额。
// 超限返回 429，IP 进 10–15 分钟的惩罚区，屡犯永久封禁。请求一律经 [`auth::query_request`] /
// [`auth::action_request`] 发，按地址取额度。
// ---------------------------------------------------------------------------

use crate::exchange::rate_limit::{Quota, RateLimiter, Use};
use std::time::Duration;

/// 本进程访问 IBKR 的唯一限频闸门
pub(crate) static LIMITER: std::sync::LazyLock<RateLimiter> =
    std::sync::LazyLock::new(|| RateLimiter::new(crate::domain::Exchange::IBKR));
/// 全局：10 次 / 秒
static GLOBAL: Quota = Quota::new("全局", 10, Duration::from_secs(1));
/// `GET /iserver/account/orders`：1 次 / 5 秒
static ACCOUNT_ORDERS: Quota = Quota::new("iserver/account/orders", 1, Duration::from_secs(5));
/// `/portfolio/accounts`：1 次 / 5 秒
static PORTFOLIO_ACCOUNTS: Quota = Quota::new("portfolio/accounts", 1, Duration::from_secs(5));
/// `/tickle`：1 次 / 秒
static TICKLE: Quota = Quota::new("tickle", 1, Duration::from_secs(1));

/// 一个请求要花的额度：全局 1 次，命中有单独限额的接口再加那一项。`url` 是完整地址。
pub(crate) fn request_costs(method: &str, url: &str) -> Vec<Use<'static>> {
    let path = url.split('?').next().unwrap_or(url).trim_end_matches('/');
    let mut costs = vec![GLOBAL.cost(1)];
    if method == "GET" && path.ends_with("/iserver/account/orders") {
        costs.push(ACCOUNT_ORDERS.cost(1));
    } else if path.ends_with("/portfolio/accounts") {
        costs.push(PORTFOLIO_ACCOUNTS.cost(1));
    } else if path.ends_with("/tickle") {
        costs.push(TICKLE.cost(1));
    }
    costs
}

fn default_gateway_url() -> String {
    "https://localhost:5000/v1/api/".to_string()
}

/// IBKR 凭证 (tagged enum: OAuth 或 Gateway)
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "mode")]
pub enum IbkrCredentials {
    #[serde(rename = "oauth")]
    OAuth {
        consumer_key: String,
        encryption_key_fp: String,
        signature_key_fp: String,
        access_token: String,
        access_token_secret: String,
        dh_prime: String,
        symbols: Vec<String>,
    },
    #[serde(rename = "gateway")]
    Gateway {
        #[serde(default = "default_gateway_url")]
        base_url: String,
        symbols: Vec<String>,
    },
}

impl IbkrCredentials {
    /// 获取 symbol 列表
    pub fn symbols(&self) -> &[String] {
        match self {
            Self::OAuth { symbols, .. } => symbols,
            Self::Gateway { symbols, .. } => symbols,
        }
    }

    /// 创建对应模式的认证器
    pub async fn create_auth(&self) -> anyhow::Result<Arc<dyn IbkrAuth>> {
        match self {
            Self::OAuth {
                consumer_key,
                encryption_key_fp,
                signature_key_fp,
                access_token,
                access_token_secret,
                dh_prime,
                ..
            } => {
                let oauth = IbkrOAuth::create(
                    consumer_key,
                    encryption_key_fp,
                    signature_key_fp,
                    access_token,
                    access_token_secret,
                    dh_prime,
                )
                .await?;
                Ok(Arc::new(oauth) as Arc<dyn IbkrAuth>)
            }
            Self::Gateway { base_url, .. } => {
                Ok(Arc::new(IbkrGateway::new(base_url.clone())) as Arc<dyn IbkrAuth>)
            }
        }
    }
}

/// 本所适配层是否实现了这种公共订阅（**能力查询**，装配期由 `ExchangeSetup` 携带）。
/// IBKR 没有 kind 映射函数（smd 报文按 conid 拼装），public WS 只实现了 BBO —— 手写并钉测试。
pub fn supports_subscription(kind: &crate::exchange::SubscriptionKind) -> bool {
    matches!(kind, crate::exchange::SubscriptionKind::BBO { .. })
}

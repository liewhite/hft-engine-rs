pub(crate) mod actor;
mod client;
pub(crate) mod codec;
mod symbol;

pub use actor::{OkxArgs, OkxNode};
pub(crate) use actor::{OkxGreeksPoller, GREEKS_POLLING_INTERVAL_MS};
pub use client::{OkxAccount, OkxClient};
pub use symbol::{from_okx, from_okx_index, to_okx, to_okx_index};

use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// 本引擎在 OKX 的订单号：`x` + 31 hex（OKX 不允许以数字开头，至多 32 位）
pub const CLIENT_ORDER_IDS: super::CliOrderIdFormat = super::CliOrderIdFormat::new("x", 31);

/// OKX REST API 地址
pub const REST_BASE_URL: &str = "https://www.okx.com";

// ---------------------------------------------------------------------------
// 限频（#791，机制见 crate::exchange::rate_limit）。OKX 按接口各自限额，数字取自官方 v5 文档
// （2026-11 实查）；规则写着「按品种」的用 `cost_in(instId)` 分开计。
// ---------------------------------------------------------------------------

use crate::exchange::rate_limit::{Quota, RateLimiter};
use std::time::Duration;

const TWO_SECS: Duration = Duration::from_secs(2);

/// 本进程访问 OKX 的唯一限频闸门（IP 与账户额度在进程内共用）
pub(crate) static LIMITER: std::sync::LazyLock<RateLimiter> =
    std::sync::LazyLock::new(|| RateLimiter::new(crate::domain::Exchange::OKX));
/// `public/instruments`：20 次 / 2 秒（IP + 品种类型；只查 SWAP）
pub(crate) static INSTRUMENTS: Quota = Quota::new("instruments", 20, TWO_SECS);
/// `account/greeks`：10 次 / 2 秒（账户）
pub(crate) static GREEKS: Quota = Quota::new("account/greeks", 10, TWO_SECS);
/// `account/balance`：10 次 / 2 秒（账户）
pub(crate) static BALANCE: Quota = Quota::new("account/balance", 10, TWO_SECS);
/// `account/positions`：10 次 / 2 秒（账户）
pub(crate) static POSITIONS: Quota = Quota::new("account/positions", 10, TWO_SECS);
/// `account/bills`：5 次 / 秒（账户）
pub(crate) static BILLS: Quota = Quota::new("account/bills", 5, Duration::from_secs(1));
/// `trade/orders-pending`：60 次 / 2 秒（账户）
pub(crate) static ORDERS_PENDING: Quota = Quota::new("orders-pending", 60, TWO_SECS);
/// `trade/order`：60 次 / 2 秒（账户 + 品种）
pub(crate) static PLACE: Quota = Quota::new("trade/order", 60, TWO_SECS);
/// 子账户下单总数：1000 次 / 2 秒（账户，所有品种合计）
pub(crate) static PLACE_ACCOUNT: Quota = Quota::new("账户下单总数", 1000, TWO_SECS);
/// `trade/cancel-order`：60 次 / 2 秒（账户 + 品种）
pub(crate) static CANCEL: Quota = Quota::new("cancel-order", 60, TWO_SECS);
/// `public/funding-rate-history`：10 次 / 2 秒（IP + 品种）
pub(crate) static FUNDING_HISTORY: Quota = Quota::new("funding-rate-history", 10, TWO_SECS);
/// `market/candles`：40 次 / 2 秒（IP）
pub(crate) static CANDLES: Quota = Quota::new("market/candles", 40, TWO_SECS);

/// OKX WebSocket 地址
pub const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
pub const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";
pub const WS_BUSINESS_URL: &str = "wss://ws.okx.com:8443/ws/v5/business";

/// WS 保活：文本 "ping"，服务端 30 秒无消息断开
pub(crate) const KEEPALIVE: crate::exchange::ws_loop::WsKeepalive =
    crate::exchange::ws_loop::WsKeepalive {
        ping_text: Some("ping"),
        ping_interval: Duration::from_secs(15),
        idle_timeout: Duration::from_secs(60),
    };

/// OKX 凭证
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl OkxCredentials {
    /// OKX 签名：`base64(HMAC-SHA256(secret, prehash))`。REST（`OkxAccount::signed`）与私有 WS
    /// 登录共用这一处，各自只拼自己的 prehash。
    pub(crate) fn sign(&self, prehash: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .expect("HMAC 接受任意长度的密钥");
        mac.update(prehash.as_bytes());
        general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    /// WebSocket 登录签名
    pub fn sign_ws_login(&self, timestamp: &str) -> String {
        self.sign(&format!("{timestamp}GET/users/self/verify"))
    }
}

/// 本所适配层是否实现了这种公共订阅（**能力查询**，装配期由 `ExchangeSetup` 携带）。
///
/// 派生自 public WS 的 kind 映射函数；一个手写例外并注明依据：Candle 不在 public 线的
/// `kind_to_arg` 里 —— 由 business WS 承接（见 `actor::business_ws`），故显式补真。
pub fn supports_subscription(kind: &crate::exchange::SubscriptionKind) -> bool {
    actor::public_ws::kind_to_arg(kind, "USDT").is_some()
        || matches!(kind, crate::exchange::SubscriptionKind::Candle { .. })
}

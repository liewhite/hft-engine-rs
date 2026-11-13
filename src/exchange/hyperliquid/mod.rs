pub(crate) mod actor;
mod client;
pub(crate) mod codec;
pub(crate) mod markets;
pub(crate) mod signing;
#[cfg(test)]
pub(crate) mod testkit;
mod transport;

pub use actor::{HyperliquidArgs, HyperliquidNode};
pub use client::{HyperliquidAccount, HyperliquidClient};

/// 本引擎在 Hyperliquid 的订单号（cloid）：协议要求 128-bit 十六进制，即 `0x` + 32 hex。
pub const CLIENT_ORDER_IDS: super::CliOrderIdFormat = super::CliOrderIdFormat::new("0x", 32);

/// Hyperliquid REST API 地址
pub const REST_BASE_URL: &str = "https://api.hyperliquid.xyz";

// ---------------------------------------------------------------------------
// 限频（#791，机制见 crate::exchange::rate_limit）。数字取自官方文档 Rate limits and user limits
// （2026-11 实查）：REST 按 IP 共用 1200 权重 / 分钟；`/exchange` 每个动作 1，`/info` 按类型 2 / 20 / 60，
// 部分接口按返回条数另加（见 client 的 `info_weight`）。
//
// 按地址的动作额度（累计成交 1 USDC 换 1 次、起始 1 万次，撤单另有更宽的额度，超了每 10 秒放 1 次）
// 不在本地计：它是随成交回补的累计余额，超了交易所只是放慢、不封禁，本地照搬还得启动时查余额、
// 按成交回补，多一套机制换不来什么。
// ---------------------------------------------------------------------------

/// 本进程访问 HL 的唯一限频闸门（经签名机提交的动作也从本机 IP 发出，同样计入）
pub(crate) static LIMITER: std::sync::LazyLock<crate::exchange::rate_limit::RateLimiter> =
    std::sync::LazyLock::new(|| {
        crate::exchange::rate_limit::RateLimiter::new(crate::domain::Exchange::Hyperliquid)
    });
/// 按 IP 的 REST 权重：1200 / 分钟
pub(crate) static IP_WEIGHT: crate::exchange::rate_limit::Quota =
    crate::exchange::rate_limit::Quota::new(
        "IP 权重/分钟",
        1200,
        std::time::Duration::from_secs(60),
    );

/// Hyperliquid WebSocket 地址
pub const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";

/// WS 保活：`{"method":"ping"}`，服务端 60 秒无消息断开
pub(crate) const KEEPALIVE: crate::exchange::ws_loop::WsKeepalive =
    crate::exchange::ws_loop::WsKeepalive {
        ping_text: Some(r#"{"method":"ping"}"#),
        ping_interval: std::time::Duration::from_secs(30),
        idle_timeout: std::time::Duration::from_secs(90),
    };

/// Hyperliquid 的接入配置（与其他所的 `ExchangeAccess` 同形，多一个 `dexes`）。
///
/// ```json
/// {"quote": "USDC", "dexes": ["", "xyz"], "credentials": {...}}
/// ```
/// `dexes`：接入的 perp dex（`""` = 主站，`"xyz"` = 股票），不写 = 只接主站。
/// 专属结构而不是给通用 `ExchangeAccess` 加字段：其他所配了 `dexes` 由
/// `deny_unknown_fields` 当场报错，不靠各装配函数记得检查。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HyperliquidAccess {
    pub quote: String,
    #[serde(default = "main_dex_only")]
    pub dexes: Vec<String>,
    /// None = 只接公共行情
    #[serde(default)]
    pub credentials: Option<HyperliquidCredentials>,
}

fn main_dex_only() -> Vec<String> {
    vec![String::new()]
}

/// Hyperliquid 凭证：查询用的主钱包地址 + 下单怎么签名 + 市价单保护价。
///
/// 配置写法（签名二选一，两个都写或都不写在加载时报错）：
/// ```json
/// {"wallet_address": "0x...", "private_key": "...", "market_order_slippage": 0.05}
/// {"wallet_address": "0x...", "signer_url": "http://127.0.0.1:8080", "market_order_slippage": 0.05}
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(try_from = "RawHyperliquidCredentials")]
pub struct HyperliquidCredentials {
    /// 主钱包地址 (0x...)。持仓、挂单、成交等查询都用它，不是 agent 地址。
    pub wallet_address: String,
    pub signing: HyperliquidSigning,
    /// 市价单的保护价让幅：HL 没有原生市价单，框架用「对手价 ×(1 ± 本值)」的 IOC 限价单代替，
    /// 只吃这个范围内的流动性。允许多大滑点是使用方的选择，必填、框架不给默认值；须在 (0, 1)。
    /// 加载时校验并转成精确十进制（下单路径直接用，不再转换、不再判非法）。
    pub(crate) market_order_slippage: rust_decimal::Decimal,
}

impl HyperliquidCredentials {
    /// 本进程持有 agent 私钥（与从配置加载同一套校验）
    pub fn with_private_key(
        wallet_address: String,
        private_key: String,
        market_order_slippage: f64,
    ) -> Result<Self, String> {
        Self::try_from(RawHyperliquidCredentials {
            wallet_address,
            private_key: Some(private_key),
            signer_url: None,
            market_order_slippage,
        })
    }

    /// 用同机签名服务（与从配置加载同一套校验）
    pub fn with_signer(
        wallet_address: String,
        url: String,
        market_order_slippage: f64,
    ) -> Result<Self, String> {
        Self::try_from(RawHyperliquidCredentials {
            wallet_address,
            private_key: None,
            signer_url: Some(url),
            market_order_slippage,
        })
    }
}

/// 下单签名方式。切换只改配置，下单逻辑不变（见 `transport` 模块）。
#[derive(Clone)]
pub enum HyperliquidSigning {
    /// 本进程持有 agent 私钥（不含 0x 前缀亦可）
    PrivateKey(String),
    /// 同机 enclave-hyperliquid-signer 的 hostd 地址
    Signer { url: String },
}

/// 手写 Debug：私钥不进日志
impl std::fmt::Debug for HyperliquidSigning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HyperliquidSigning::PrivateKey(_) => f.write_str("PrivateKey(<redacted>)"),
            HyperliquidSigning::Signer { url } => {
                f.debug_struct("Signer").field("url", url).finish()
            }
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHyperliquidCredentials {
    wallet_address: String,
    private_key: Option<String>,
    signer_url: Option<String>,
    market_order_slippage: f64,
}

impl TryFrom<RawHyperliquidCredentials> for HyperliquidCredentials {
    type Error = String;
    fn try_from(r: RawHyperliquidCredentials) -> Result<Self, String> {
        let signing = match (r.private_key, r.signer_url) {
            (Some(k), None) => HyperliquidSigning::PrivateKey(k),
            (None, Some(url)) => HyperliquidSigning::Signer { url },
            (Some(_), Some(_)) => {
                return Err("hyperliquid 凭证 private_key 与 signer_url 只能写一个".into())
            }
            (None, None) => return Err("hyperliquid 凭证缺 private_key 或 signer_url".into()),
        };
        let market_order_slippage = crate::domain::from_f64(r.market_order_slippage)
            .filter(|s| *s > rust_decimal::Decimal::ZERO && *s < rust_decimal::Decimal::ONE)
            .ok_or_else(|| {
                format!(
                    "hyperliquid 凭证 market_order_slippage 须在 (0, 1)，当前 {}",
                    r.market_order_slippage
                )
            })?;
        Ok(Self {
            wallet_address: r.wallet_address,
            signing,
            market_order_slippage,
        })
    }
}

/// 本所适配层是否实现了这种公共订阅（**能力查询**，装配期由 `ExchangeSetup` 携带）。
/// 派生自 public WS 的 kind 映射函数，见各所同名函数的约定。
pub fn supports_subscription(kind: &crate::exchange::SubscriptionKind) -> bool {
    actor::public_ws::kind_channel(kind).is_some()
}

#[cfg(test)]
mod credentials_tests {
    use super::*;

    #[test]
    fn exactly_one_signing_method() {
        let k: HyperliquidCredentials = serde_json::from_str(
            r#"{"wallet_address":"0xa","private_key":"ab","market_order_slippage":0.05}"#,
        )
        .unwrap();
        assert!(matches!(k.signing, HyperliquidSigning::PrivateKey(_)));
        let s: HyperliquidCredentials =
            serde_json::from_str(r#"{"wallet_address":"0xa","signer_url":"http://127.0.0.1:8080","market_order_slippage":0.05}"#).unwrap();
        assert!(matches!(s.signing, HyperliquidSigning::Signer { .. }));
        assert!(serde_json::from_str::<HyperliquidCredentials>(
            r#"{"wallet_address":"0xa","private_key":"ab","signer_url":"x"}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<HyperliquidCredentials>(r#"{"wallet_address":"0xa"}"#).is_err()
        );
    }

    #[test]
    fn private_key_is_not_debug_printed() {
        let c: HyperliquidCredentials = serde_json::from_str(
            r#"{"wallet_address":"0xa","private_key":"deadbeef","market_order_slippage":0.05}"#,
        )
        .unwrap();
        assert!(!format!("{c:?}").contains("deadbeef"));
    }
}

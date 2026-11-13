//! Hyperliquid 下单 / 撤单的**签名与提交**通道。
//!
//! # 为什么单独抽出来
//!
//! 同一个账户有两种下单方式，区别只在"谁持有交易 key、谁签名"：
//! - [`LocalKeySubmitter`]：本进程持有 agent 私钥，自己构造 action、算 nonce、EIP-712 签名、
//!   POST 到 HL `/exchange`；
//! - [`EnclaveSubmitter`]：私钥只在同机 Nitro enclave 签名机里（enclave-hyperliquid-signer），
//!   本进程只把**结构化意图**（币名、方向、价格、数量……）POST 给 hostd，由它按内置风控签名并
//!   提交，返回 HL 的原始回复。
//!
//! 两者对 [`HyperliquidClient`](super::HyperliquidClient) 的契约相同：给一个意图，返回 HL 的
//! 原始回复 JSON（`{"status": ..., "response": ...}`）。回复的解读（statuses 逐单判错）只在
//! client 里写一份，两种通道共用。切换通道只改配置（见 [`super::HyperliquidSigning`]），
//! 下单逻辑一行不动。
//!
//! # 意图里为什么同时带 `coin` 和 `asset`
//!
//! 本地签名要资产编号（action 里是 `a`），签名机只收币名（编号由它按编进镜像的白名单换算，
//! 宿主机给的编号它不信）。编号由 client 从 meta 解析一次、缓存，两个字段各取所需。

use super::signing::{
    action_hash, create_signer, sign_l1_action, BulkOrderAction, CancelByCloidAction, CancelByCloidWire,
    ExchangeRequest, LimitOrder, OrderType as WireOrderType, OrderWire,
};
use crate::domain::{now_ms, Exchange, ExchangeError, PlaceError};
use crate::exchange::client::place_send_error;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// HL 的 time-in-force（wire 上的写法）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlTif {
    /// Add Liquidity Only（只做 maker）
    Alo,
    Ioc,
    Gtc,
}

impl HlTif {
    fn as_str(self) -> &'static str {
        match self {
            HlTif::Alo => "Alo",
            HlTif::Ioc => "Ioc",
            HlTif::Gtc => "Gtc",
        }
    }
}

/// 一笔限价单意图。价格与数量已是 HL 规范小数字符串（见 [`float_to_wire`]）。
#[derive(Debug, Clone)]
pub(crate) struct HlOrderIntent {
    /// HL 币名，带 dex 前缀（如 `BTC`、`xyz:TSLA`）
    pub coin: String,
    /// 全局资产编号（含 dex 偏移）
    pub asset: u32,
    pub is_buy: bool,
    pub limit_px: String,
    pub sz: String,
    pub reduce_only: bool,
    pub tif: HlTif,
    /// `0x` + 32 位小写十六进制
    pub cloid: Option<String>,
}

/// 一笔撤单意图（按客户端订单号 cloid：`0x` + 32 位小写十六进制）
#[derive(Debug, Clone)]
pub(crate) struct HlCancelIntent {
    pub coin: String,
    pub asset: u32,
    pub cloid: String,
}

/// 签名并提交到 HL 的通道。返回 HL `/exchange` 的原始回复。
///
/// 拿不到回复时的失败按「确定没提交 / 结果不确定」归类（见 [`PlaceError`]）：只有通道自己
/// 知道请求走到了哪一步（签名前失败、连接没建立、签名机拒签、已签名但提交超时……），所以
//// 在这里定。拿到回复之后的解读（statuses 逐单判错）在 client。撤单不分这两类（撤单失败只回报
/// 给策略，单在不在以交易所随后的订单回报为准），按普通错误返回。
#[async_trait]
pub(crate) trait HlSubmitter: Send + Sync {
    async fn place(&self, order: &HlOrderIntent) -> Result<Value, PlaceError>;
    async fn cancel(&self, cancel: &HlCancelIntent) -> Result<Value, ExchangeError>;
}

/// 价格 / 数量 → HL 规范小数字符串，与 hyperliquid-python-sdk 的 `float_to_wire` 同口径：
/// 最多 8 位小数、无多余前后导零、不用科学计数法；四舍五入到 8 位会改变数值则报错
/// （说明上游没有按精度取整，宁可拒单也不悄悄改价）。签名机对不规范的写法直接拒签，
/// 本地签名路径也用同一份，免得两条通道对同一笔单写出不同的字符串。
pub(crate) fn float_to_wire(x: f64) -> Result<String, ExchangeError> {
    if !x.is_finite() || x < 0.0 {
        return Err(ExchangeError::Other(format!("HL 价格/数量非法: {x}")));
    }
    let rounded = format!("{x:.8}");
    let back: f64 = rounded.parse().map_err(|_| ExchangeError::ParseError(rounded.clone()))?;
    if (back - x).abs() >= 1e-12 {
        return Err(ExchangeError::Other(format!("HL 价格/数量超过 8 位小数: {x}")));
    }
    let s = rounded.trim_end_matches('0').trim_end_matches('.');
    Ok(if s.is_empty() || s == "-0" { "0".to_string() } else { s.to_string() })
}

// ---------------------------------------------------------------------------
// 本地私钥
// ---------------------------------------------------------------------------

/// 本进程持有 agent 私钥，自己签名并 POST 到 HL。
pub(crate) struct LocalKeySubmitter {
    http: Client,
    base_url: String,
    signer: PrivateKeySigner,
    is_mainnet: bool,
}

impl LocalKeySubmitter {
    pub(crate) fn new(
        http: Client,
        base_url: String,
        private_key: &str,
        is_mainnet: bool,
    ) -> Result<Self, ExchangeError> {
        let signer = create_signer(private_key)
            .map_err(|e| ExchangeError::Other(format!("Failed to create signer: {e}")))?;
        Ok(Self { http, base_url, signer, is_mainnet })
    }

    async fn sign_and_post<A: serde::Serialize>(&self, action: &A) -> Result<Value, PlaceError> {
        let nonce = next_nonce(now_ms());
        let connection_id = action_hash(action, nonce, None)
            .map_err(|e| PlaceError::unsent(format!("Action hash failed: {e}")))?;
        let signature = sign_l1_action(&self.signer, connection_id, self.is_mainnet)
            .await
            .map_err(|e| PlaceError::unsent(format!("Signing failed: {e}")))?;
        let request = ExchangeRequest {
            action: serde_json::to_value(action)
                .map_err(|e| PlaceError::unsent(format!("Serialize action failed: {e}")))?,
            nonce,
            signature: signature.to_api_format(),
            vault_address: None,
        };
        post_json(&self.http, &format!("{}/exchange", self.base_url), &request).await
    }
}

#[async_trait]
impl HlSubmitter for LocalKeySubmitter {
    async fn place(&self, o: &HlOrderIntent) -> Result<Value, PlaceError> {
        let action = BulkOrderAction::new(vec![OrderWire {
            asset: o.asset,
            is_buy: o.is_buy,
            limit_px: o.limit_px.clone(),
            sz: o.sz.clone(),
            reduce_only: o.reduce_only,
            order_type: WireOrderType::Limit(LimitOrder { tif: o.tif.as_str().to_string() }),
            cloid: o.cloid.clone(),
        }]);
        self.sign_and_post(&action).await
    }

    async fn cancel(&self, c: &HlCancelIntent) -> Result<Value, ExchangeError> {
        let action = CancelByCloidAction::new(vec![CancelByCloidWire { asset: c.asset, cloid: c.cloid.clone() }]);
        self.sign_and_post(&action).await.map_err(cancel_error)
    }
}

/// 本进程签出的上一个 nonce（见 [`next_nonce`]）。单测会把它推到墙钟之后数小时，HL 允许领先 1 天，无害
static LAST_NONCE: AtomicU64 = AtomicU64::new(0);

/// 下一个签名 nonce：`max(当前毫秒, 上一个 + 1)`。
///
/// HL 要求同一签名地址的 nonce 不重复（且落在最近 100 个最大值之上）。每笔下单 / 撤单各在
/// 自己的任务里并发签名，同一毫秒签两笔时直接取毫秒会撞号、后到的被拒。取整个进程一个计数
/// （而不是每个签名者一个）：一个进程就算有多个持同一私钥的客户端也不会撞，严格递增对 HL
/// 也总是合法的。只在每秒超过 1000 笔时才会领先墙钟，HL 允许领先 1 天。
fn next_nonce(now: u64) -> u64 {
    let mut last = LAST_NONCE.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST_NONCE.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

/// 撤单不区分两类失败，转成普通错误（只为日志可读）
fn cancel_error(e: PlaceError) -> ExchangeError {
    ExchangeError::Other(format!("Hyperliquid cancel: {e}"))
}

/// POST JSON 到 HL `/exchange`，HTTP 层的失败按 [`classify_hl_http`] 归类。
async fn post_json<B: serde::Serialize>(http: &Client, url: &str, body: &B) -> Result<Value, PlaceError> {
    let resp = http
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| place_send_error(Exchange::Hyperliquid, e))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| PlaceError::indeterminate(format!("HL 读回复失败 (HTTP {status}): {e}")))?;
    classify_hl_http(status, &text)
}

/// HL `/exchange` 的 HTTP 层：2xx 且是 JSON → 交给 client 解读；4xx（422 请求体不合法、
/// 429 限频）→ HL 没受理，拒绝；408（超时）、5xx 或 2xx 却不是 JSON → 不确定。纯函数。
fn classify_hl_http(status: u16, text: &str) -> Result<Value, PlaceError> {
    let body: String = text.chars().take(300).collect();
    match status {
        200..=299 => serde_json::from_str(text)
            .map_err(|e| PlaceError::indeterminate(format!("HL /exchange 回复不是 JSON: {e} (body: {body})"))),
        400..=499 if status != 408 => Err(PlaceError::rejected(format!("HL HTTP {status}: {body}"))),
        _ => Err(PlaceError::indeterminate(format!("HL HTTP {status}: {body}"))),
    }
}

// ---------------------------------------------------------------------------
// enclave 签名机
// ---------------------------------------------------------------------------

/// 把意图交给同机签名机（hostd，默认 `http://127.0.0.1:8080`）签名并提交。
///
/// 接口约定见 enclave-hyperliquid-signer `docs/integration.md`：请求体
/// `{"intent": {...}}`，回复 `{"signed":..,"response":<HL 原始回复>,"error":..}`。
pub(crate) struct EnclaveSubmitter {
    http: Client,
    url: String,
}

impl EnclaveSubmitter {
    pub(crate) fn new(http: Client, url: String) -> Self {
        Self { http, url: url.trim_end_matches('/').to_string() }
    }

    async fn exchange(&self, intent: Value) -> Result<Value, PlaceError> {
        let resp = self
            .http
            .post(format!("{}/exchange", self.url))
            .json(&json!({ "intent": intent }))
            .send()
            .await
            // 连不上 hostd（is_connect）= 请求没送到，确定没提交；其余可能已提交
            .map_err(|e| place_send_error(Exchange::Hyperliquid, e))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| PlaceError::indeterminate(format!("signer 读回复失败 (HTTP {status}): {e}")))?;
        let body: Value = serde_json::from_str(&text).map_err(|e| {
            PlaceError::indeterminate(format!("signer 回复不是 JSON: {e} (HTTP {status}, body: {text})"))
        })?;
        classify_signer_reply(status, body)
    }
}

/// 按签名机的 HTTP 状态归类（表见 integration.md 第 4 节）。纯函数，便于测试。
fn classify_signer_reply(status: u16, body: Value) -> Result<Value, PlaceError> {
    let err = body.get("error").and_then(Value::as_str).unwrap_or("").to_string();
    match status {
        // 200 却没有 response：签名机可能已提交，结果不确定
        200 => body
            .get("response")
            .cloned()
            .ok_or_else(|| PlaceError::indeterminate(format!("signer 200 但没有 response: {body}"))),
        // 违反风控 / 格式错：确定没签、没提交
        422 => Err(PlaceError::rejected(format!("signer 拒签: {err}"))),
        // 已签名但提交 HL 失败（有 signed）：单可能已到 HL，结果不确定
        502 if body.get("signed").is_some() => Err(PlaceError::indeterminate(format!(
            "signer 已签名但提交 HL 失败: {err}"
        ))),
        // 连不上 enclave（无 signed）：没签名就不可能提交
        502 => Err(PlaceError::unsent(format!("signer 连不上 enclave: {err}"))),
        // 其余（500 审计失败等）：说不清走到了哪一步
        _ => Err(PlaceError::indeterminate(format!("signer HTTP {status}: {err}"))),
    }
}

#[async_trait]
impl HlSubmitter for EnclaveSubmitter {
    async fn place(&self, o: &HlOrderIntent) -> Result<Value, PlaceError> {
        let mut order = json!({
            "coin": o.coin,
            "isBuy": o.is_buy,
            "sz": o.sz,
            "limitPx": o.limit_px,
            "orderType": {"limit": {"tif": o.tif.as_str()}},
            "reduceOnly": o.reduce_only,
        });
        if let Some(cloid) = &o.cloid {
            order["cloid"] = json!(cloid);
        }
        self.exchange(json!({"type": "order", "grouping": "na", "orders": [order]})).await
    }

    async fn cancel(&self, c: &HlCancelIntent) -> Result<Value, ExchangeError> {
        self.exchange(json!({"type": "cancelByCloid", "cancels": [{"coin": c.coin, "cloid": c.cloid}]}))
            .await
            .map_err(cancel_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_wire_is_canonical() {
        assert_eq!(float_to_wire(65000.0).unwrap(), "65000");
        assert_eq!(float_to_wire(0.001).unwrap(), "0.001");
        assert_eq!(float_to_wire(1.10).unwrap(), "1.1");
        assert_eq!(float_to_wire(0.0).unwrap(), "0");
        assert_eq!(float_to_wire(0.1 + 0.2).unwrap(), "0.3");
        assert_eq!(float_to_wire(0.00000001).unwrap(), "0.00000001");
    }

    #[test]
    fn float_to_wire_rejects_what_would_be_silently_changed() {
        assert!(float_to_wire(0.123456789).is_err());
        assert!(float_to_wire(f64::NAN).is_err());
        assert!(float_to_wire(-1.0).is_err());
    }

    #[test]
    fn signer_200_yields_raw_hl_reply() {
        let hl = json!({"status": "ok", "response": {"type": "order"}});
        let got = classify_signer_reply(200, json!({"signed": {}, "response": hl.clone()})).unwrap();
        assert_eq!(got, hl);
    }

    #[test]
    fn signer_policy_rejection_is_rejected() {
        let e = classify_signer_reply(422, json!({"error": "coin not allowed"})).unwrap_err();
        assert!(matches!(e, PlaceError::Rejected(_)), "{e:?}");
    }

    /// 已签名提交失败、200 没 response、500 → 不确定；连不上 enclave（没签名）→ 确定没提交
    #[test]
    fn signer_failures_split_by_whether_it_may_have_been_submitted() {
        for (status, body) in [
            (502, json!({"signed": {"nonce": 1}, "error": "timeout"})),
            (500, json!({"error": "audit"})),
            (200, json!({"signed": {}})),
        ] {
            let e = classify_signer_reply(status, body.clone()).unwrap_err();
            assert!(matches!(e, PlaceError::Indeterminate(_)), "{status} {body}: {e:?}");
        }
        let e = classify_signer_reply(502, json!({"error": "vsock"})).unwrap_err();
        assert!(matches!(e, PlaceError::Rejected(_)), "{e:?}");
    }

    /// 同一毫秒并发签名也拿到互不相同、严格递增的 nonce；墙钟往前走时跟上墙钟
    #[test]
    fn nonces_never_repeat_within_a_millisecond() {
        let now = now_ms() + 10_000_000; // 远高于别的测试可能用过的值
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(move || (0..1000).map(|_| next_nonce(now)).collect::<Vec<_>>()))
            .collect();
        let mut all: Vec<u64> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n, "有重复 nonce");
        assert!(all[0] >= now);
        let later = all[n - 1] + 1_000;
        assert_eq!(next_nonce(later), later, "墙钟超过计数后应取墙钟");
    }

    #[test]
    fn hl_http_layer() {
        assert_eq!(classify_hl_http(200, r#"{"status":"ok"}"#).unwrap(), json!({"status": "ok"}));
        for (status, text) in [(200, "<html>"), (408, ""), (500, "oops"), (502, ""), (504, "")] {
            assert!(matches!(classify_hl_http(status, text), Err(PlaceError::Indeterminate(_))), "{status}");
        }
        for (status, text) in [(422, "Failed to deserialize the JSON body"), (429, "rate limited")] {
            assert!(matches!(classify_hl_http(status, text), Err(PlaceError::Rejected(_))), "{status}");
        }
    }
}

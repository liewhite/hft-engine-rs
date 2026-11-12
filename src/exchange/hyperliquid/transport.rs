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
    action_hash, create_signer, sign_l1_action, BulkCancelAction, BulkOrderAction, CancelWire,
    ExchangeRequest, LimitOrder, OrderType as WireOrderType, OrderWire,
};
use crate::domain::{now_ms, Exchange, ExchangeError, RejectReason};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

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

/// 一笔撤单意图（按交易所侧 oid）
#[derive(Debug, Clone)]
pub(crate) struct HlCancelIntent {
    pub coin: String,
    pub asset: u32,
    pub oid: u64,
}

/// 签名并提交到 HL 的通道。返回 HL `/exchange` 的原始回复。
///
/// 错误语义（调用方据此判断这单**是否可能已到 HL**）：
/// - `ConnectionFailed` / `Timeout`：结果未知，可能已提交（与网络断开同一类）；
/// - `OrderRejected`：确定没有提交（签名机按风控拒签等）；
/// - 其余：确定没有提交。
#[async_trait]
pub(crate) trait HlSubmitter: Send + Sync {
    async fn place(&self, order: &HlOrderIntent) -> Result<Value, ExchangeError>;
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

    async fn sign_and_post<A: serde::Serialize>(&self, action: &A) -> Result<Value, ExchangeError> {
        let nonce = now_ms();
        let connection_id = action_hash(action, nonce, None)
            .map_err(|e| ExchangeError::Other(format!("Action hash failed: {e}")))?;
        let signature = sign_l1_action(&self.signer, connection_id, self.is_mainnet)
            .await
            .map_err(|e| ExchangeError::Other(format!("Signing failed: {e}")))?;
        let request = ExchangeRequest {
            action: serde_json::to_value(action)
                .map_err(|e| ExchangeError::Other(format!("Serialize action failed: {e}")))?,
            nonce,
            signature: signature.to_api_format(),
            vault_address: None,
        };
        post_json(&self.http, &format!("{}/exchange", self.base_url), &request).await
    }
}

#[async_trait]
impl HlSubmitter for LocalKeySubmitter {
    async fn place(&self, o: &HlOrderIntent) -> Result<Value, ExchangeError> {
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
        let action = BulkCancelAction::new(vec![CancelWire { a: c.asset, o: c.oid }]);
        self.sign_and_post(&action).await
    }
}

/// POST JSON，HTTP 非 2xx 报 `ApiError`，网络错误报 `ConnectionFailed`（结果未知）。
async fn post_json<B: serde::Serialize>(http: &Client, url: &str, body: &B) -> Result<Value, ExchangeError> {
    let resp = http
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;
    if !status.is_success() {
        return Err(ExchangeError::ApiError(Exchange::Hyperliquid, status.as_u16() as i32, text));
    }
    serde_json::from_str(&text)
        .map_err(|e| ExchangeError::ParseError(format!("HL /exchange 回复不是 JSON: {e} (body: {text})")))
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

    async fn exchange(&self, intent: Value) -> Result<Value, ExchangeError> {
        let resp = self
            .http
            .post(format!("{}/exchange", self.url))
            .json(&json!({ "intent": intent }))
            .send()
            .await
            // 连不上 hostd：请求没送到，确定没有提交；但读回复中途断开时可能已提交，
            // 两者在 reqwest 层分不开，一律按"结果未知"报。
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, format!("signer: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, format!("signer: {e}")))?;
        let body: Value = serde_json::from_str(&text)
            .map_err(|e| ExchangeError::ParseError(format!("signer 回复不是 JSON: {e} (HTTP {status}, body: {text})")))?;
        classify_signer_reply(status, body)
    }
}

/// 按签名机的 HTTP 状态归类（表见 integration.md 第 4 节）。纯函数，便于测试。
fn classify_signer_reply(status: u16, body: Value) -> Result<Value, ExchangeError> {
    let err = body.get("error").and_then(Value::as_str).unwrap_or("").to_string();
    match status {
        200 => body
            .get("response")
            .cloned()
            .ok_or_else(|| ExchangeError::ParseError(format!("signer 200 但没有 response: {body}"))),
        // 违反风控 / 格式错：确定没签、没提交
        422 => Err(ExchangeError::OrderRejected(
            Exchange::Hyperliquid,
            RejectReason::Other(format!("signer 拒签: {err}")),
        )),
        // 已签名但提交 HL 失败（有 signed）：单可能已到 HL；或连不上 enclave（无 signed）。
        // 前者结果未知，后者确定没提交 —— 按更坏的"未知"报，调用方靠回报流 / 查单收敛。
        502 if body.get("signed").is_some() => Err(ExchangeError::ConnectionFailed(
            Exchange::Hyperliquid,
            format!("signer 已签名但提交 HL 失败，结果未知: {err}"),
        )),
        _ => Err(ExchangeError::ApiError(Exchange::Hyperliquid, status as i32, format!("signer: {err}"))),
    }
}

#[async_trait]
impl HlSubmitter for EnclaveSubmitter {
    async fn place(&self, o: &HlOrderIntent) -> Result<Value, ExchangeError> {
        let mut order = json!({
            "coin": o.coin,
            "isBuy": o.is_buy,
            "sz": o.sz,
            "limitPx": o.limit_px,
            "orderType": {"limit": {"tif": o.tif.as_str()}},
            "reduceOnly": o.reduce_only,
        });
        if let Some(cloid) = &o.cloid {
            order["cloid"] = json!(cloid.to_lowercase());
        }
        self.exchange(json!({"type": "order", "grouping": "na", "orders": [order]})).await
    }

    async fn cancel(&self, c: &HlCancelIntent) -> Result<Value, ExchangeError> {
        self.exchange(json!({"type": "cancel", "cancels": [{"coin": c.coin, "oid": c.oid}]}))
            .await
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
    fn signer_policy_rejection_is_order_rejected() {
        let e = classify_signer_reply(422, json!({"error": "coin not allowed"})).unwrap_err();
        assert!(matches!(e, ExchangeError::OrderRejected(Exchange::Hyperliquid, _)), "{e:?}");
    }

    #[test]
    fn signer_submit_failure_after_signing_is_unknown_outcome() {
        let e = classify_signer_reply(502, json!({"signed": {"nonce": 1}, "error": "timeout"})).unwrap_err();
        assert!(matches!(e, ExchangeError::ConnectionFailed(..)), "{e:?}");
        let e = classify_signer_reply(502, json!({"error": "vsock"})).unwrap_err();
        assert!(matches!(e, ExchangeError::ApiError(_, 502, _)), "{e:?}");
        let e = classify_signer_reply(500, json!({"error": "audit"})).unwrap_err();
        assert!(matches!(e, ExchangeError::ApiError(_, 500, _)), "{e:?}");
    }
}

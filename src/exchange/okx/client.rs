//! OKX ExchangeClient 实现 (仅 REST)

use super::symbol::{from_okx, to_okx};
use crate::domain::{
    Exchange, ExchangeError, Greeks, OrderId, OrderStatus, OrderType, OrderUpdate, Side,
    Symbol, SymbolMeta, TimeInForce, now_ms,
};
use crate::exchange::okx::codec::{GreeksData, PositionData};
use crate::exchange::client::{ExchangeClient, ExchangeOrder};
pub use crate::exchange::okx::OkxCredentials;
use crate::exchange::okx::REST_BASE_URL;
use crate::exchange::utils::StepFormatter;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use reqwest::Client;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

/// OKX 交易所客户端
pub struct OkxClient {
    /// symbol -> 合约乘数 (ctVal) 缓存，惰性拉取一次
    ///
    /// [`ExchangeClient`] 的契约是**返回币本位数量** (见 [`crate::domain::Quantity`])，而 OKX
    /// REST 返回的是张数，故 client 必须自备折算能力，不能把折算责任推给调用方 —— 那样每个
    /// 调用点都得记着乘一次，漏一处就静默算错 contract_size 倍。
    contract_sizes: tokio::sync::OnceCell<HashMap<Symbol, f64>>,
    /// HTTP 客户端
    client: Client,
    /// 凭证（可选）
    credentials: Option<OkxCredentials>,
    /// REST API 基础 URL
    base_url: String,
    /// 计价币种 (e.g., "USDT")
    quote: String,
}

impl OkxClient {
    /// 创建新的 OKX 客户端
    pub fn new(quote: String, credentials: Option<OkxCredentials>) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        Ok(Self {
            contract_sizes: tokio::sync::OnceCell::new(),
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
            quote,
        })
    }

    /// 取该 symbol 的合约乘数 (张 -> 币)，首次调用时拉取并缓存全部合约信息。
    async fn contract_size_of(&self, symbol: &Symbol) -> Result<f64, ExchangeError> {
        let sizes = self
            .contract_sizes
            .get_or_try_init(|| async {
                let metas = self.get_all_instruments().await?;
                Ok::<_, ExchangeError>(
                    metas
                        .into_iter()
                        .map(|m| (m.symbol, m.contract_size))
                        .collect::<HashMap<_, _>>(),
                )
            })
            .await?;
        sizes.get(symbol).copied().ok_or_else(|| {
            ExchangeError::Other(format!("No OKX contract size for symbol {symbol}"))
        })
    }

    /// 获取计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 获取凭证（供 ManagerActor 创建 OkxActor 使用）
    pub fn credentials(&self) -> Option<&OkxCredentials> {
        self.credentials.as_ref()
    }

    /// reqwest 错误转换
    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string())
    }

    /// ISO 8601 格式时间戳
    fn iso_timestamp() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    /// REST API 签名
    fn sign(&self, timestamp: &str, method: &str, path: &str, body: &str) -> Option<String> {
        let credentials = self.credentials.as_ref()?;
        let message = format!("{}{}{}{}", timestamp, method, path, body);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(credentials.secret.as_bytes()).ok()?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        Some(general_purpose::STANDARD.encode(result.into_bytes()))
    }

    /// 构建请求头
    fn build_headers(&self, sign: &str, timestamp: &str) -> Option<HeaderMap> {
        let credentials = self.credentials.as_ref()?;
        let mut headers = HeaderMap::new();
        headers.insert("OK-ACCESS-KEY", credentials.api_key.parse().ok()?);
        headers.insert("OK-ACCESS-SIGN", sign.parse().ok()?);
        headers.insert("OK-ACCESS-TIMESTAMP", timestamp.parse().ok()?);
        headers.insert("OK-ACCESS-PASSPHRASE", credentials.passphrase.parse().ok()?);
        headers.insert("Content-Type", "application/json".parse().ok()?);
        Some(headers)
    }

    /// 获取所有交易对信息 (公开接口)
    async fn get_all_instruments(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<InstrumentData>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InstrumentData {
            inst_id: String,
            tick_sz: String,
            lot_sz: String,
            min_sz: String,
            ct_val: String,
        }

        let resp = self
            .client
            .get(format!(
                "{}/api/v5/public/instruments?instType=SWAP",
                self.base_url
            ))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        let metas: Vec<SymbolMeta> = data
            .data
            .into_iter()
            .filter_map(|d| {
                // 只处理配置的 quote 对应的品种 (e.g., BTC-USDT-SWAP)
                if !d.inst_id.contains(&format!("-{}-SWAP", self.quote)) {
                    return None;
                }
                let symbol = from_okx(&d.inst_id)?;
                let price_step: f64 = d.tick_sz.parse().ok().filter(|&v| v > 0.0)?;
                let size_step: f64 = d.lot_sz.parse().ok().filter(|&v| v > 0.0)?;
                let min_order_size: f64 = d.min_sz.parse().ok().filter(|&v| v > 0.0)?;
                let contract_size: f64 = d.ct_val.parse().ok().filter(|&v| v > 0.0)?;

                Some(SymbolMeta {
                    exchange: Exchange::OKX,
                    symbol,
                    price_formatter: Arc::new(StepFormatter::new(price_step)),
                    size_step,
                    min_order_size,
                    contract_size,
                })
            })
            .collect();

        Ok(metas)
    }

    /// 查询账户希腊值 (GET /api/v5/account/greeks)
    pub async fn fetch_greeks(&self) -> Result<Vec<Greeks>, ExchangeError> {
        let path = "/api/v5/account/greeks";
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<GreeksData>,
        }

        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        let data: Response = serde_json::from_str(&text)
            .map_err(|e| ExchangeError::Other(format!(
                "Failed to parse greeks response: {}, body: {}", e, text
            )))?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        let mut result = Vec::new();
        for item in &data.data {
            let greeks = item.to_greeks()
                .map_err(|e| ExchangeError::Other(e))?;
            result.push(greeks);
        }

        Ok(result)
    }
}

#[async_trait]
impl ExchangeClient for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        self.get_all_instruments().await
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.get_all_instruments().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all.into_iter().filter(|m| symbol_set.contains(&m.symbol)).collect())
    }

    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError> {
        let path = "/api/v5/trade/cancel-order";
        let inst_id = to_okx(symbol, &self.quote);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct CancelRequest {
            inst_id: String,
            ord_id: String,
        }

        let request = CancelRequest {
            inst_id,
            ord_id: order_id.clone(),
        };

        let body = serde_json::to_string(&request)?;
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "POST", path, &body)
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CancelData {
            s_code: String,
            s_msg: String,
        }

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<CancelData>,
        }

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if let Some(cancel_data) = data.data.first() {
            if cancel_data.s_code != "0" {
                return Err(ExchangeError::ApiError(
                    Exchange::OKX,
                    cancel_data.s_code.parse().unwrap_or(-1),
                    cancel_data.s_msg.clone(),
                ));
            }
            return Ok(());
        }

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        Ok(())
    }

    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError> {
        // 无凭证 = 只接公共行情（见 ExchangeAccess），没有账户就没有挂单 —— 与
        // fetch_positions 同一口径。若在此返回 Err，`ManagerActor` 启动期会**逐 symbol**
        // 各报一次错（模拟盘跑几百个 symbol 就是几百条 warn），噪音会盖住真实告警。
        //
        // 注意：place_order / cancel_order / set_leverage 不该这样处理 —— 那些是"没凭证就
        // 真的做不到"，报错是正确行为。只有只读查询才能把"没有账户"表达成空结果。
        if self.credentials.is_none() {
            return Ok(Vec::new());
        }

        let inst_id = to_okx(symbol, &self.quote);
        let path = format!(
            "/api/v5/trade/orders-pending?instId={}&instType=SWAP",
            inst_id
        );
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", &path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PendingOrderData {
            inst_id: String,
            ord_id: String,
            cl_ord_id: Option<String>,
            side: String,
            state: String,
            /// 订单价格
            px: String,
            /// 订单数量 (张)
            sz: String,
            acc_fill_sz: String,
        }

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<PendingOrderData>,
        }

        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        // 张 -> 币：REST 全程用张数，此处一次取到乘数，下方所有数量字段统一折算
        let contract_size = self.contract_size_of(symbol).await?;

        let mut updates = Vec::new();
        for d in &data.data {
            let sym = match from_okx(&d.inst_id) {
                Some(s) => s,
                None => {
                    tracing::warn!(inst_id = %d.inst_id, "Unknown inst_id in pending orders, skipping");
                    continue;
                }
            };
            let side = match d.side.as_str() {
                "buy" => Side::Long,
                "sell" => Side::Short,
                other => {
                    tracing::warn!(side = %other, ord_id = %d.ord_id, "Unknown side in pending order, skipping");
                    continue;
                }
            };
            let acc_fill_sz: f64 = match d.acc_fill_sz.parse::<f64>().map(|v| v * contract_size) {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(ord_id = %d.ord_id, acc_fill_sz = %d.acc_fill_sz, "Failed to parse acc_fill_sz, skipping");
                    continue;
                }
            };
            let status = match d.state.as_str() {
                "live" => OrderStatus::Pending,
                "partially_filled" => OrderStatus::PartiallyFilled { filled: acc_fill_sz },
                other => {
                    tracing::warn!(state = %other, ord_id = %d.ord_id, "Unexpected state in pending orders, skipping");
                    continue;
                }
            };

            // 用 cl_ord_id 或 fallback 到 ord_id，确保 pending_orders 能跟踪
            let client_order_id = d
                .cl_ord_id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| d.ord_id.clone());

            let price: f64 = d.px.parse()
                .map_err(|_| ExchangeError::Other(format!("Failed to parse px '{}' for order {}", d.px, d.ord_id)))?;
            let sz: f64 = d.sz.parse::<f64>()
                .map(|v| v * contract_size)
                .map_err(|_| ExchangeError::Other(format!("Failed to parse sz '{}' for order {}", d.sz, d.ord_id)))?;

            updates.push(OrderUpdate {
                order_id: d.ord_id.clone(),
                client_order_id: Some(client_order_id),
                exchange: Exchange::OKX,
                symbol: sym,
                side,
                status,
                price,
                reduce_only: false, // OKX REST pending 响应未解析 reduceOnly，外部单元信息以 WS 推送为准
                quantity: sz,
                filled_quantity: acc_fill_sz,
                fill_sz: 0.0,
                timestamp: now_ms(),
            });
        }

        Ok(updates)
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let path = "/api/v5/trade/order";
        let inst_id = to_okx(&order.symbol, &self.quote);
        let side = side_to_okx(order.side);
        let (ord_type, price) = order_type_to_okx(&order.order_type);
        let sz = order.quantity.to_string();
        let reduce_only = order.reduce_only;

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct OrderRequest {
            inst_id: String,
            td_mode: String,
            side: String,
            ord_type: String,
            sz: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            px: Option<String>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            reduce_only: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            cl_ord_id: Option<String>,
        }

        let request = OrderRequest {
            inst_id,
            td_mode: "cross".to_string(),
            side: side.to_string(),
            ord_type: ord_type.to_string(),
            sz,
            px: price,
            reduce_only,
            cl_ord_id: if order.client_order_id.is_empty() {
                None
            } else {
                Some(order.client_order_id.clone())
            },
        };

        let body = serde_json::to_string(&request)?;
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "POST", path, &body)
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OrderData {
            ord_id: String,
            s_code: String,
            s_msg: String,
        }

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<OrderData>,
        }

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        // 先检查 data 中的具体错误信息（更详细）
        if let Some(order_data) = data.data.first() {
            if order_data.s_code != "0" {
                let msg = format!("code={}, msg={}", order_data.s_code, order_data.s_msg);
                return Err(ExchangeError::OrderRejected(
                    Exchange::OKX,
                    crate::domain::RejectReason::classify(&msg),
                ));
            }
            return Ok(order_data.ord_id.clone());
        }

        // 如果没有 data，检查顶层错误
        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        Err(ExchangeError::OrderRejected(
            Exchange::OKX,
            crate::domain::RejectReason::Other("No order data in response".to_string()),
        ))
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        let path = "/api/v5/account/set-leverage";
        let inst_id = to_okx(symbol, &self.quote);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request {
            inst_id: String,
            lever: String,
            mgn_mode: String,
        }

        let request = Request {
            inst_id,
            lever: leverage.to_string(),
            mgn_mode: "cross".to_string(),
        };

        let body = serde_json::to_string(&request)?;
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "POST", path, &body)
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
        }

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        Ok(())
    }

    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        // OKX 的 equity/notional 经私有 WS 推送（AccountInfo 事件），本方法没有实现完整
        // 语义（此前 notional 硬编码 0.0）。返回编造的数字比报错危险得多 —— 下游拿它算
        // account_leverage = notional / equity 会静默失效。诚实报不支持，调用方要么走
        // WS 推送，要么在这里补齐真实实现。
        Err(ExchangeError::Other(
            "OKX 不支持 REST fetch_account_info（equity/notional 走私有 WS 推送），\
             拒绝返回编造的数字"
                .to_string(),
        ))
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        // 无凭证 = 只接公共行情（见 ExchangeAccess），没有账户可查，空仓是事实而非缺数据
        if self.credentials.is_none() {
            return Ok(Vec::new());
        }

        let path = "/api/v5/account/positions?instType=SWAP";
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<PositionData>,
        }

        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        let mut positions = Vec::new();
        for d in &data.data {
            let Some(symbol) = from_okx(&d.inst_id) else {
                // 账户里可能有本项目未接入的合约（如交割合约），跳过而非报错
                tracing::debug!(inst_id = %d.inst_id, "OKX position 的 instId 无法识别，跳过");
                continue;
            };

            // 张 -> 币：[`ExchangeClient`] 的契约是返回币本位（见 crate::domain::Quantity）。
            // 字段口径（空串=0、张数、双向持仓拒绝）全在 codec 里，不在此处重抄一份。
            let contract_size = self.contract_size_of(&symbol).await?;
            positions.push(
                d.to_position(symbol, contract_size)
                    .map_err(ExchangeError::ParseError)?,
            );
        }

        Ok(positions)
    }
}

/// 错误码映射
fn map_okx_error(code: &str, msg: &str) -> ExchangeError {
    match code {
        "50013" => ExchangeError::RateLimited(Exchange::OKX, Duration::from_secs(60)),
        "51020" => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            format!("Position limit exceeded: {}", msg),
        ),
        "51121" => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            format!("Order quantity exceeded: {}", msg),
        ),
        _ => ExchangeError::ApiError(Exchange::OKX, code.parse().unwrap_or(-1), msg.to_string()),
    }
}

/// Side 转换
fn side_to_okx(side: Side) -> &'static str {
    match side {
        Side::Long => "buy",
        Side::Short => "sell",
    }
}

/// OrderType 转换
fn order_type_to_okx(order_type: &OrderType) -> (&'static str, Option<String>) {
    match order_type {
        OrderType::Market => ("market", None),
        OrderType::Limit { price, tif } => {
            let ord_type = match tif {
                TimeInForce::GTC => "limit",
                TimeInForce::IOC => "ioc",
                TimeInForce::FOK => "fok",
                TimeInForce::PostOnly => "post_only",
            };
            (ord_type, Some(price.to_string()))
        }
    }
}

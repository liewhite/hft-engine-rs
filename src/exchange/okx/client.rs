//! OKX ExchangeClient 实现 (仅 REST)

use super::symbol::{from_okx, to_okx};
use crate::domain::{
    Exchange, ExchangeError, Order, OrderId, OrderStatus, OrderType, OrderUpdate, Side, Symbol,
    SymbolMeta, TimeInForce, now_ms,
};
use crate::exchange::client::ExchangeClient;
pub use crate::exchange::okx::OkxCredentials;
use crate::exchange::okx::REST_BASE_URL;
use crate::exchange::utils::StepFormatter;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

/// OKX 交易所客户端
pub struct OkxClient {
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
    pub fn new(credentials: Option<OkxCredentials>) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        let quote = credentials
            .as_ref()
            .map(|c| c.quote.clone())
            .unwrap_or_else(|| "USDT".to_string());

        Ok(Self {
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
            quote,
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

    /// 查询账户净值 (totalEq)
    async fn get_equity(&self) -> Result<f64, ExchangeError> {
        let path = "/api/v5/account/balance";
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct BalanceData {
            total_eq: String,
        }

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<BalanceData>,
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

        let balance_data = data
            .data
            .first()
            .ok_or_else(|| ExchangeError::Other("No balance data in response".to_string()))?;

        let equity: f64 = balance_data
            .total_eq
            .parse()
            .map_err(|_| ExchangeError::Other("Failed to parse totalEq".to_string()))?;

        Ok(equity)
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
            let acc_fill_sz: f64 = match d.acc_fill_sz.parse() {
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
            let sz: f64 = d.sz.parse()
                .map_err(|_| ExchangeError::Other(format!("Failed to parse sz '{}' for order {}", d.sz, d.ord_id)))?;

            updates.push(OrderUpdate {
                order_id: d.ord_id.clone(),
                client_order_id: Some(client_order_id),
                exchange: Exchange::OKX,
                symbol: sym,
                side,
                status,
                price,
                quantity: sz,          // 张数，由 manager 转换为币
                filled_quantity: acc_fill_sz, // 张数，由 manager 转换为币
                fill_sz: 0.0,
                timestamp: now_ms(),
            });
        }

        Ok(updates)
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
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
                Some(order.client_order_id)
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
                return Err(ExchangeError::OrderRejected(
                    Exchange::OKX,
                    format!("code={}, msg={}", order_data.s_code, order_data.s_msg),
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
            "No order data in response".to_string(),
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

    async fn fetch_account_info(&self) -> Result<crate::exchange::AccountInfo, ExchangeError> {
        // OKX 通过 WebSocket 推送 equity 和 notional，这里仅实现 trait
        // 实际使用中不会调用此方法
        let equity = self.get_equity().await?;
        Ok(crate::exchange::AccountInfo {
            equity,
            notional: 0.0, // OKX 通过 WebSocket 推送 notional
        })
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

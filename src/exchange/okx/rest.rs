use crate::domain::{Exchange, ExchangeError, Order, OrderId, OrderType, Side, Symbol, SymbolMeta, TimeInForce};
use crate::exchange::api::ExchangeExecutor;
use crate::exchange::okx::REST_BASE_URL;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::Duration;

/// OKX REST API 客户端
pub struct OkxRestClient {
    client: Client,
    api_key: String,
    secret: String,
    passphrase: String,
    base_url: String,
}

impl OkxRestClient {
    pub fn new(api_key: String, secret: String, passphrase: String) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            secret,
            passphrase,
            base_url: REST_BASE_URL.to_string(),
        })
    }

    /// reqwest 错误转换
    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string())
    }

    /// 获取 API Key
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// 获取 Passphrase
    pub fn passphrase(&self) -> &str {
        &self.passphrase
    }

    /// ISO 8601 格式时间戳
    fn iso_timestamp() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    /// Unix 时间戳 (秒)
    pub fn unix_timestamp() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }

    /// REST API 签名
    fn sign(&self, timestamp: &str, method: &str, path: &str, body: &str) -> String {
        let message = format!("{}{}{}{}", timestamp, method, path, body);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any size");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        general_purpose::STANDARD.encode(result.into_bytes())
    }

    /// WebSocket 登录签名
    pub fn sign_ws_login(&self, timestamp: &str) -> String {
        let message = format!("{}GET/users/self/verify", timestamp);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any size");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        general_purpose::STANDARD.encode(result.into_bytes())
    }

    /// 构建请求头
    fn build_headers(&self, sign: &str, timestamp: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("OK-ACCESS-KEY", self.api_key.parse().unwrap());
        headers.insert("OK-ACCESS-SIGN", sign.parse().unwrap());
        headers.insert("OK-ACCESS-TIMESTAMP", timestamp.parse().unwrap());
        headers.insert("OK-ACCESS-PASSPHRASE", self.passphrase.parse().unwrap());
        headers.insert("Content-Type", "application/json".parse().unwrap());
        headers
    }

    /// 获取交易所交易对信息 (公开接口)
    pub async fn get_instruments(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
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

        // 构建需要查询的 symbol 集合
        let symbol_set: std::collections::HashSet<_> = symbols.iter()
            .map(|s| s.to_okx())
            .collect();

        let metas: Vec<SymbolMeta> = data
            .data
            .into_iter()
            .filter(|d| symbol_set.contains(&d.inst_id))
            .filter_map(|d| {
                let symbol = Symbol::from_okx(&d.inst_id)?;
                let price_step: f64 = d.tick_sz.parse().ok().filter(|&v| v > 0.0)?;
                let size_step: f64 = d.lot_sz.parse().ok().filter(|&v| v > 0.0)?;
                let min_order_size: f64 = d.min_sz.parse().unwrap_or(0.0);
                let contract_size: f64 = d.ct_val.parse().ok().filter(|&v| v > 0.0)?;

                Some(SymbolMeta {
                    exchange: Exchange::OKX,
                    symbol,
                    price_step,
                    size_step,
                    min_order_size,
                    contract_size,
                })
            })
            .collect();

        Ok(metas)
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

#[async_trait]
impl ExchangeExecutor for OkxRestClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        self.get_instruments(symbols).await
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
        let path = "/api/v5/trade/order";
        let inst_id = order.symbol.to_okx();
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
            cl_ord_id: order.client_order_id,
        };

        let body = serde_json::to_string(&request)?;
        let timestamp = Self::iso_timestamp();
        let sign = self.sign(&timestamp, "POST", path, &body);
        let headers = self.build_headers(&sign, &timestamp);

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OrderData {
            ord_id: String,
            #[allow(dead_code)]
            cl_ord_id: Option<String>,
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

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        let order_data = data.data.first().ok_or_else(|| {
            ExchangeError::OrderRejected(Exchange::OKX, "No order data in response".to_string())
        })?;

        if order_data.s_code != "0" {
            return Err(ExchangeError::OrderRejected(
                Exchange::OKX,
                order_data.s_msg.clone(),
            ));
        }

        Ok(OrderId::from(order_data.ord_id.clone()))
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        let path = "/api/v5/account/set-leverage";
        let inst_id = symbol.to_okx();

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
        let sign = self.sign(&timestamp, "POST", path, &body);
        let headers = self.build_headers(&sign, &timestamp);

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

    async fn fetch_equity(&self) -> Result<f64, ExchangeError> {
        let path = "/api/v5/account/balance";
        let timestamp = Self::iso_timestamp();
        let sign = self.sign(&timestamp, "GET", path, "");
        let headers = self.build_headers(&sign, &timestamp);

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

        let balance_data = data.data.first().ok_or_else(|| {
            ExchangeError::Other("No balance data in response".to_string())
        })?;

        let equity: f64 = balance_data
            .total_eq
            .parse()
            .map_err(|_| ExchangeError::Other("Failed to parse totalEq".to_string()))?;

        Ok(equity)
    }
}

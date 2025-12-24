use crate::domain::{Exchange, ExchangeError, Order, OrderId, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::binance::REST_BASE_URL;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::time::Duration;

/// Binance REST API 客户端
pub struct BinanceRestClient {
    client: Client,
    api_key: String,
    secret: String,
    base_url: String,
}

impl BinanceRestClient {
    pub fn new(api_key: String, secret: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            api_key,
            secret,
            base_url: REST_BASE_URL.to_string(),
        }
    }

    /// 签名
    fn sign(&self, query_string: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any size");
        mac.update(query_string.as_bytes());
        let result = mac.finalize();
        hex::encode(result.into_bytes())
    }

    /// 构建带签名的请求参数
    fn build_signed_query(&self, params: &[(&str, &str)]) -> String {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let mut query_parts: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        query_parts.push(format!("timestamp={}", timestamp));

        let query_string = query_parts.join("&");
        let signature = self.sign(&query_string);

        format!("{}&signature={}", query_string, signature)
    }

    /// 创建 ListenKey (用于私有 WebSocket)
    pub async fn create_listen_key(&self) -> Result<String, ExchangeError> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "listenKey")]
            listen_key: String,
        }

        let resp = self
            .client
            .post(format!("{}/fapi/v1/listenKey", self.base_url))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                status.as_u16() as i32,
                text,
            )));
        }

        let data: Response = resp.json().await?;
        Ok(data.listen_key)
    }

    /// 续期 ListenKey
    pub async fn keep_alive_listen_key(&self) -> Result<(), ExchangeError> {
        let resp = self
            .client
            .put(format!("{}/fapi/v1/listenKey", self.base_url))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                -1,
                text,
            )));
        }

        Ok(())
    }

    /// 下单
    pub async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
        let symbol = order.symbol.to_binance();
        let side = side_to_binance(order.side);
        let (order_type, price, tif) = order_type_to_binance(&order.order_type);
        let qty = order.quantity.0.to_string();
        let reduce_only = if order.reduce_only { "true" } else { "false" };

        let mut params: Vec<(&str, &str)> = vec![
            ("symbol", &symbol),
            ("side", side),
            ("type", order_type),
            ("quantity", &qty),
            ("reduceOnly", reduce_only),
        ];

        let price_str;
        if let Some(p) = price {
            price_str = p;
            params.push(("price", &price_str));
        }

        if let Some(t) = tif {
            params.push(("timeInForce", t));
        }

        if let Some(ref coid) = order.client_order_id {
            params.push(("newClientOrderId", coid));
        }

        let query = self.build_signed_query(&params);

        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "orderId")]
            order_id: i64,
        }

        let resp = self
            .client
            .post(format!("{}/fapi/v1/order?{}", self.base_url, query))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::OrderRejected(
                Exchange::Binance,
                text,
            )));
        }

        let data: Response = resp.json().await?;
        Ok(OrderId::from(data.order_id))
    }

    /// 设置杠杆
    pub async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        let symbol_str = symbol.to_binance();
        let leverage_str = leverage.to_string();
        let params = [("symbol", symbol_str.as_str()), ("leverage", &leverage_str)];
        let query = self.build_signed_query(&params);

        let resp = self
            .client
            .post(format!("{}/fapi/v1/leverage?{}", self.base_url, query))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                -1,
                text,
            )));
        }

        Ok(())
    }

    /// 解析错误响应
    fn parse_error(&self, text: &str) -> Option<ExchangeError> {
        #[derive(Deserialize)]
        struct ErrorResponse {
            code: i32,
            msg: String,
        }

        let err: ErrorResponse = serde_json::from_str(text).ok()?;
        Some(map_binance_error(err.code, &err.msg))
    }
}

/// 错误码映射
fn map_binance_error(code: i32, msg: &str) -> ExchangeError {
    match code {
        -1003 => ExchangeError::RateLimited(Exchange::Binance, Duration::from_secs(60)),
        -2010 | -2019 => {
            ExchangeError::InsufficientBalance(Exchange::Binance, rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
        }
        -4028 => ExchangeError::ApiError(
            Exchange::Binance,
            code,
            format!("Leverage exceeded: {}", msg),
        ),
        _ => ExchangeError::ApiError(Exchange::Binance, code, msg.to_string()),
    }
}

/// Side 转换
fn side_to_binance(side: Side) -> &'static str {
    match side {
        Side::Long => "BUY",
        Side::Short => "SELL",
    }
}

/// OrderType 转换
fn order_type_to_binance(order_type: &OrderType) -> (&'static str, Option<String>, Option<&'static str>) {
    match order_type {
        OrderType::Market => ("MARKET", None, None),
        OrderType::Limit { price, tif } => {
            let tif_str = match tif {
                TimeInForce::GTC => "GTC",
                TimeInForce::IOC => "IOC",
                TimeInForce::FOK => "FOK",
                TimeInForce::PostOnly => "GTX",
            };
            ("LIMIT", Some(price.0.to_string()), Some(tif_str))
        }
    }
}

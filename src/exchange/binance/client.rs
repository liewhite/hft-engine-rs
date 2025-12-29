//! Binance ExchangeClient 实现

use crate::domain::{
    Exchange, ExchangeError, Order, OrderId, OrderType, Side, Symbol, SymbolMeta, TimeInForce,
};
use crate::exchange::binance::actor::{BinanceActor, BinanceActorArgs};
pub use crate::exchange::binance::actor::BinanceCredentials;
use crate::exchange::binance::REST_BASE_URL;
use crate::exchange::client::{ExchangeClient, ExchangeClientHandle, MarketDataSink, Subscribe, SubscriptionKind, Unsubscribe};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use kameo::actor::{ActorID, ActorRef};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Binance 交易所客户端
pub struct BinanceClient {
    /// HTTP 客户端
    client: Client,
    /// 凭证（可选）
    credentials: Option<BinanceCredentials>,
    /// REST API 基础 URL
    base_url: String,
}

impl BinanceClient {
    /// 创建新的 Binance 客户端
    pub fn new(credentials: Option<BinanceCredentials>) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

        Ok(Self {
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
        })
    }

    /// 获取 API Key（如果有）
    fn api_key(&self) -> Option<&str> {
        self.credentials.as_ref().map(|c| c.api_key.as_str())
    }

    /// 获取 Secret（如果有）
    fn secret(&self) -> Option<&str> {
        self.credentials.as_ref().map(|c| c.secret.as_str())
    }

    /// reqwest 错误转换
    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string())
    }

    /// 签名
    fn sign(&self, query_string: &str) -> Option<String> {
        let secret = self.secret()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).ok()?;
        mac.update(query_string.as_bytes());
        let result = mac.finalize();
        Some(hex::encode(result.into_bytes()))
    }

    /// 构建带签名的请求参数
    fn build_signed_query(&self, params: &[(&str, &str)]) -> Option<String> {
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
        let signature = self.sign(&query_string)?;

        Some(format!("{}&signature={}", query_string, signature))
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

    /// 获取交易所交易对信息
    async fn get_exchange_info(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        #[derive(Deserialize)]
        struct ExchangeInfo {
            symbols: Vec<SymbolInfo>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SymbolInfo {
            symbol: String,
            filters: Vec<Filter>,
        }

        #[derive(Deserialize)]
        #[serde(tag = "filterType")]
        enum Filter {
            #[serde(rename = "PRICE_FILTER")]
            PriceFilter {
                #[serde(rename = "tickSize")]
                tick_size: String,
            },
            #[serde(rename = "LOT_SIZE")]
            LotSize {
                #[serde(rename = "stepSize")]
                step_size: String,
                #[serde(rename = "minQty")]
                min_qty: String,
            },
            #[serde(other)]
            Other,
        }

        let resp = self
            .client
            .get(format!("{}/fapi/v1/exchangeInfo", self.base_url))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                status.as_u16() as i32,
                text,
            )));
        }

        let info: ExchangeInfo = resp.json().await.map_err(Self::map_reqwest_error)?;

        // 构建需要查询的 symbol 集合
        let symbol_set: std::collections::HashSet<_> =
            symbols.iter().map(|s| s.to_binance()).collect();

        let metas: Vec<SymbolMeta> = info
            .symbols
            .into_iter()
            .filter(|s| symbol_set.contains(&s.symbol))
            .filter_map(|s| {
                let symbol = Symbol::from_binance(&s.symbol)?;
                let mut price_step: Option<f64> = None;
                let mut size_step: Option<f64> = None;
                let mut min_order_size: Option<f64> = None;

                for filter in s.filters {
                    match filter {
                        Filter::PriceFilter { tick_size } => {
                            price_step = tick_size.parse().ok();
                        }
                        Filter::LotSize { step_size, min_qty } => {
                            size_step = step_size.parse().ok();
                            min_order_size = min_qty.parse().ok();
                        }
                        Filter::Other => {}
                    }
                }

                let price_step = price_step.filter(|&v| v > 0.0)?;
                let size_step = size_step.filter(|&v| v > 0.0)?;
                let min_order_size = min_order_size.unwrap_or(0.0);

                Some(SymbolMeta {
                    exchange: Exchange::Binance,
                    symbol,
                    price_step,
                    size_step,
                    min_order_size,
                    contract_size: 1.0,
                })
            })
            .collect();

        Ok(metas)
    }

    /// 查询账户净值 (totalMarginBalance)
    async fn get_equity(&self) -> Result<f64, ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AccountInfo {
            total_margin_balance: String,
        }

        let query = self
            .build_signed_query(&[])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .get(format!("{}/fapi/v2/account?{}", self.base_url, query))
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                status.as_u16() as i32,
                text,
            )));
        }

        let account: AccountInfo = resp.json().await.map_err(Self::map_reqwest_error)?;
        let equity: f64 = account
            .total_margin_balance
            .parse()
            .map_err(|_| ExchangeError::Other("Failed to parse totalMarginBalance".to_string()))?;

        Ok(equity)
    }
}

#[async_trait]
impl ExchangeClient for BinanceClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        self.get_exchange_info(symbols).await
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        let symbol = order.symbol.to_binance();
        let side = side_to_binance(order.side);
        let (order_type, price, tif) = order_type_to_binance(&order.order_type);
        let qty = order.quantity.to_string();
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

        let query = self
            .build_signed_query(&params)
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "orderId")]
            order_id: i64,
        }

        let resp = self
            .client
            .post(format!("{}/fapi/v1/order?{}", self.base_url, query))
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self
                .parse_error(&text)
                .unwrap_or(ExchangeError::OrderRejected(Exchange::Binance, text)));
        }

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;
        Ok(data.order_id.to_string())
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        let symbol_str = symbol.to_binance();
        let leverage_str = leverage.to_string();
        let params = [("symbol", symbol_str.as_str()), ("leverage", &leverage_str)];
        let query = self
            .build_signed_query(&params)
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .post(format!("{}/fapi/v1/leverage?{}", self.base_url, query))
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

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

    async fn fetch_equity(&self) -> Result<f64, ExchangeError> {
        self.get_equity().await
    }

    async fn start(
        self: Arc<Self>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        data_sink: Arc<dyn MarketDataSink>,
    ) -> Result<Arc<dyn ExchangeClientHandle>, ExchangeError> {
        let actor = BinanceActor::new(BinanceActorArgs {
            credentials: self.credentials.clone(),
            symbol_metas,
            data_sink,
            rest_base_url: self.base_url.clone(),
            api_key: self.api_key().map(|s| s.to_string()),
        });

        let actor_ref = kameo::spawn(actor);
        Ok(Arc::new(BinanceClientHandle { actor_ref }))
    }
}

/// Binance 客户端句柄
struct BinanceClientHandle {
    actor_ref: ActorRef<BinanceActor>,
}

#[async_trait]
impl ExchangeClientHandle for BinanceClientHandle {
    fn actor_id(&self) -> ActorID {
        self.actor_ref.id()
    }

    async fn subscribe(&self, kind: SubscriptionKind) {
        let _ = self.actor_ref.tell(Subscribe { kind }).await;
    }

    async fn unsubscribe(&self, kind: SubscriptionKind) {
        let _ = self.actor_ref.tell(Unsubscribe { kind }).await;
    }
}

/// 错误码映射
fn map_binance_error(code: i32, msg: &str) -> ExchangeError {
    match code {
        -1003 => ExchangeError::RateLimited(Exchange::Binance, Duration::from_secs(60)),
        -2010 | -2019 => ExchangeError::InsufficientBalance(Exchange::Binance, 0.0, 0.0),
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
fn order_type_to_binance(
    order_type: &OrderType,
) -> (&'static str, Option<String>, Option<&'static str>) {
    match order_type {
        OrderType::Market => ("MARKET", None, None),
        OrderType::Limit { price, tif } => {
            let tif_str = match tif {
                TimeInForce::GTC => "GTC",
                TimeInForce::IOC => "IOC",
                TimeInForce::FOK => "FOK",
                TimeInForce::PostOnly => "GTX",
            };
            ("LIMIT", Some(price.to_string()), Some(tif_str))
        }
    }
}

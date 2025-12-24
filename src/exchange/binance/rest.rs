use crate::domain::{Balance, Exchange, ExchangeError, Order, OrderId, OrderType, Position, Side, Symbol, TimeInForce};
use crate::exchange::api::ExchangeExecutor;
use crate::exchange::binance::REST_BASE_URL;
use async_trait::async_trait;
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
    pub fn new(api_key: String, secret: String) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            secret,
            base_url: REST_BASE_URL.to_string(),
        })
    }

    /// reqwest 错误转换
    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string())
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

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;
        Ok(data.listen_key)
    }

    /// 获取最近一次资金费率的结算时间
    /// 返回 HashMap<Symbol, last_funding_time_ms>
    pub async fn get_last_funding_times(
        &self,
        symbols: &[Symbol],
    ) -> Result<std::collections::HashMap<Symbol, u64>, ExchangeError> {
        use futures_util::future::join_all;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct FundingRateRecord {
            funding_time: i64,
        }

        // 并行请求所有 symbols
        let futures: Vec<_> = symbols
            .iter()
            .map(|symbol| {
                let client = &self.client;
                let base_url = &self.base_url;
                let binance_symbol = symbol.to_binance();
                let symbol = symbol.clone();

                async move {
                    let resp = client
                        .get(format!(
                            "{}/fapi/v1/fundingRate?symbol={}&limit=1",
                            base_url, binance_symbol
                        ))
                        .send()
                        .await;

                    match resp {
                        Ok(r) if r.status().is_success() => {
                            match r.json::<Vec<FundingRateRecord>>().await {
                                Ok(data) => data
                                    .first()
                                    .map(|record| (symbol, record.funding_time as u64)),
                                Err(e) => {
                                    tracing::warn!(
                                        symbol = %binance_symbol,
                                        error = %e,
                                        "Failed to parse funding rate"
                                    );
                                    None
                                }
                            }
                        }
                        Ok(r) => {
                            tracing::warn!(
                                symbol = %binance_symbol,
                                status = r.status().as_u16(),
                                "Failed to get funding rate"
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!(
                                symbol = %binance_symbol,
                                error = %e,
                                "Request failed for funding rate"
                            );
                            None
                        }
                    }
                }
            })
            .collect();

        let results = join_all(futures).await;
        let result: std::collections::HashMap<_, _> =
            results.into_iter().flatten().collect();

        Ok(result)
    }

    /// 查询账户信息 (balances + positions)
    /// 返回 (balances, positions)
    pub async fn get_account_info(&self) -> Result<(Vec<Balance>, Vec<Position>), ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AccountInfo {
            assets: Vec<AssetInfo>,
            positions: Vec<PositionInfo>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AssetInfo {
            asset: String,
            available_balance: String,
            wallet_balance: String,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PositionInfo {
            symbol: String,
            position_amt: String,
            entry_price: String,
            unrealized_profit: String,
        }

        let query = self.build_signed_query(&[]);
        let resp = self
            .client
            .get(format!("{}/fapi/v2/account?{}", self.base_url, query))
            .header("X-MBX-APIKEY", &self.api_key)
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

        // 转换 balances
        let balances: Vec<Balance> = account
            .assets
            .iter()
            .filter_map(|a| {
                let available: f64 = a.available_balance.parse().ok()?;
                let wallet: f64 = a.wallet_balance.parse().ok()?;
                // 只返回有余额的资产
                if wallet > 0.0 || available > 0.0 {
                    Some(Balance {
                        exchange: Exchange::Binance,
                        asset: a.asset.clone(),
                        available,
                        frozen: (wallet - available).max(0.0),
                    })
                } else {
                    None
                }
            })
            .collect();

        // 转换 positions (只返回有持仓的)
        let positions: Vec<Position> = account
            .positions
            .iter()
            .filter_map(|p| {
                let pos_amt: f64 = p.position_amt.parse().ok()?;
                if pos_amt.abs() < 1e-10 {
                    return None; // 无持仓
                }
                let symbol = Symbol::from_binance(&p.symbol)?;
                let entry_price: f64 = p.entry_price.parse().ok()?;
                let unrealized_pnl: f64 = p.unrealized_profit.parse().ok()?;

                let (side, size) = if pos_amt >= 0.0 {
                    (Side::Long, pos_amt)
                } else {
                    (Side::Short, pos_amt.abs())
                };

                Some(Position {
                    exchange: Exchange::Binance,
                    symbol,
                    side,
                    size,
                    entry_price,
                    leverage: 1,
                    unrealized_pnl,
                    mark_price: 0.0,
                })
            })
            .collect();

        Ok((balances, positions))
    }

    /// 续期 ListenKey
    pub async fn keep_alive_listen_key(&self) -> Result<(), ExchangeError> {
        let resp = self
            .client
            .put(format!("{}/fapi/v1/listenKey", self.base_url))
            .header("X-MBX-APIKEY", &self.api_key)
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
            ExchangeError::InsufficientBalance(Exchange::Binance, 0.0, 0.0)
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
            ("LIMIT", Some(price.to_string()), Some(tif_str))
        }
    }
}

#[async_trait]
impl ExchangeExecutor for BinanceRestClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
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
            .await
            .map_err(Self::map_reqwest_error)?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::OrderRejected(
                Exchange::Binance,
                text,
            )));
        }

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;
        Ok(data.order_id.to_string())
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError> {
        let symbol_str = symbol.to_binance();
        let leverage_str = leverage.to_string();
        let params = [("symbol", symbol_str.as_str()), ("leverage", &leverage_str)];
        let query = self.build_signed_query(&params);

        let resp = self
            .client
            .post(format!("{}/fapi/v1/leverage?{}", self.base_url, query))
            .header("X-MBX-APIKEY", &self.api_key)
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
}

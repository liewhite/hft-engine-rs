//! IBKR ExchangeClient 实现 (仅 REST)

use crate::domain::{
    Exchange, ExchangeError, Order, OrderId, OrderType, Side, Symbol, SymbolMeta, TimeInForce,
};
use crate::exchange::client::ExchangeClient;
use crate::exchange::ibkr::auth::IbkrAuth;
use crate::exchange::ibkr::symbol::resolve_conids;
use crate::exchange::ibkr::IbkrCredentials;
use crate::exchange::utils::StepFormatter;
use async_trait::async_trait;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// 下单确认消息抑制 ID 列表
const SUPPRESS_MESSAGE_IDS: &[&str] = &[
    "o163","o399", "o299", "o354", "o382", "o383", "o407", "o434", "o451", "o452", "o462", "o478",
    "o10153",
];

/// IBKR 交易所客户端
pub struct IbkrClient {
    http: Client,
    auth: Arc<dyn IbkrAuth>,
    account_id: String,
    conids: HashMap<String, i64>,
    symbols: Vec<String>,
}

impl IbkrClient {
    /// 创建并初始化 IBKR 客户端
    pub async fn new(credentials: &IbkrCredentials) -> Result<Self, ExchangeError> {
        // 1. 创建认证器
        let auth = credentials
            .create_auth()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        let http = auth
            .build_http_client()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        let base_url = auth.base_url().to_string();

        // 2. 初始化交易会话 (POST /iserver/auth/ssodh/init)
        let init_url = format!("{}iserver/auth/ssodh/init", base_url);
        let resp = Self::authed_request_static(&http, &*auth, "POST", &init_url)?
            .json(&serde_json::json!({"publish": true, "compete": true}))
            .send()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        tracing::info!(status = %resp.status(), "IBKR brokerage session init");

        // 3. 获取 account_id (GET /portfolio/accounts)
        let accounts_url = format!("{}portfolio/accounts", base_url);
        let resp = Self::authed_request_static(&http, &*auth, "GET", &accounts_url)?
            .send()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        let accounts: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        let account_id = accounts
            .first()
            .and_then(|a| a.get("accountId"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ExchangeError::ConnectionFailed(Exchange::IBKR, "No accounts found".to_string())
            })?
            .to_string();

        tracing::info!(account_id = %account_id, "IBKR account connected");

        // 4. Switch account (POST /iserver/account)
        let switch_url = format!("{}iserver/account", base_url);
        let resp = Self::authed_request_static(&http, &*auth, "POST", &switch_url)?
            .json(&serde_json::json!({"acctId": account_id}))
            .send()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ExchangeError::ConnectionFailed(
                Exchange::IBKR,
                format!("switch account failed: {}", resp.status()),
            ));
        }

        // 5. 禁用下单确认 (POST /iserver/questions/suppress)
        let suppress_url = format!("{}iserver/questions/suppress", base_url);
        match Self::authed_request_static(&http, &*auth, "POST", &suppress_url)?
            .json(&serde_json::json!({"messageIds": SUPPRESS_MESSAGE_IDS}))
            .send()
            .await
        {
            Ok(r) if !r.status().is_success() => {
                tracing::warn!(status = %r.status(), "IBKR suppress questions failed");
            }
            Err(e) => {
                tracing::warn!(error = %e, "IBKR suppress questions request failed");
            }
            _ => {
                tracing::info!("IBKR order confirmations suppressed");
            }
        }

        // 6. 解析 conids
        let symbols = credentials.symbols();
        let conids = resolve_conids(&http, &*auth, symbols)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        Ok(Self {
            http,
            auth,
            account_id,
            conids,
            symbols: symbols.to_vec(),
        })
    }

    /// 获取认证器 (Arc) 供 Actor 共享
    pub fn auth(&self) -> Arc<dyn IbkrAuth> {
        self.auth.clone()
    }

    /// 获取 conids 映射
    pub fn conids(&self) -> &HashMap<String, i64> {
        &self.conids
    }

    /// 构建带认证 header 的请求 (静态方法，用于初始化阶段)
    fn authed_request_static(
        http: &Client,
        auth: &dyn IbkrAuth,
        method: &str,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, ExchangeError> {
        let auth_header = auth
            .sign_request(method, url, None)
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        let builder = match method {
            "GET" => http.get(url),
            "POST" => http.post(url),
            "DELETE" => http.delete(url),
            _ => http.get(url),
        };

        let mut req = builder.header("User-Agent", "ibind-rs");
        if let Some(header) = auth_header {
            req = req.header("Authorization", header);
        }

        Ok(req)
    }

    /// 构建带认证 header 的请求 (实例方法)
    fn authed_request(
        &self,
        method: &str,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, ExchangeError> {
        Self::authed_request_static(&self.http, &*self.auth, method, url)
    }

    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string())
    }
}

#[async_trait]
impl ExchangeClient for IbkrClient {
    fn exchange(&self) -> Exchange {
        Exchange::IBKR
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        // IBKR 股票精度固定
        let metas: Vec<SymbolMeta> = self
            .symbols
            .iter()
            .filter(|s| self.conids.contains_key(*s))
            .map(|s| SymbolMeta {
                exchange: Exchange::IBKR,
                symbol: s.clone(),
                price_formatter: Arc::new(StepFormatter::new(0.01)),
                size_step: 1.0,
                min_order_size: 1.0,
                contract_size: 1.0,
            })
            .collect();

        Ok(metas)
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.fetch_all_symbol_metas().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all.into_iter().filter(|m| symbol_set.contains(&m.symbol)).collect())
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
        let conid = self.conids.get(&order.symbol).ok_or_else(|| {
            ExchangeError::SymbolNotFound(Exchange::IBKR, order.symbol.clone())
        })?;

        let side = match order.side {
            Side::Long => "BUY",
            Side::Short => "SELL",
        };

        let (order_type, price, tif) = match &order.order_type {
            OrderType::Market => ("MKT", None, "DAY"),
            OrderType::Limit { price, tif } => {
                let tif_str = match tif {
                    TimeInForce::GTC => "GTC",
                    TimeInForce::IOC => "IOC",
                    TimeInForce::FOK => "FOK",
                    TimeInForce::PostOnly => "GTC",
                };
                ("LMT", Some(*price), tif_str)
            }
        };

        let url = format!(
            "{}iserver/account/{}/orders",
            self.auth.base_url(),
            self.account_id
        );

        let mut order_body = serde_json::json!({
            "conidex": format!("{}@SMART", conid),
            "secType": format!("{}:STK", conid),
            "side": side,
            "quantity": order.quantity,
            "orderType": order_type,
            "tif": tif,
            "outsideRTH": false,
        });

        if let Some(px) = price {
            order_body["price"] = serde_json::json!(px);
        }

        let body = serde_json::json!({ "orders": [order_body] });

        let resp = self
            .authed_request("POST", &url)?
            .json(&body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let resp_body: serde_json::Value =
            resp.json().await.map_err(Self::map_reqwest_error)?;

        // 处理 reply 确认循环
        let order_id = self
            .handle_order_response(&resp_body)
            .await?;

        Ok(order_id)
    }

    async fn set_leverage(&self, _symbol: &Symbol, _leverage: u32) -> Result<(), ExchangeError> {
        // 股票无杠杆设置
        Ok(())
    }

    async fn fetch_account_info(&self) -> Result<crate::exchange::AccountInfo, ExchangeError> {
        let base_url = self.auth.base_url();

        // 先 receive brokerage accounts (预热缓存)
        let recv_url = format!("{}portfolio/accounts", base_url);
        if let Err(e) = self
            .authed_request("GET", &recv_url)?
            .send()
            .await
        {
            tracing::warn!(error = %e, "IBKR portfolio/accounts prefetch failed");
        }

        // 获取 account summary
        let summary_url = format!(
            "{}portfolio/{}/summary",
            base_url, self.account_id
        );

        let resp = self
            .authed_request("GET", &summary_url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let summary: serde_json::Value =
            resp.json().await.map_err(Self::map_reqwest_error)?;

        let equity = extract_summary_value(&summary, &["netliquidation", "netLiquidationValue"])
            .unwrap_or_else(|| {
                tracing::warn!(summary = %summary, "Failed to parse equity from IBKR summary");
                0.0
            });

        let notional = extract_summary_value(&summary, &["grosspositionvalue", "securitiesGVP"])
            .unwrap_or_else(|| {
                tracing::warn!(summary = %summary, "Failed to parse notional from IBKR summary");
                0.0
            })
            .abs();

        Ok(crate::exchange::AccountInfo { equity, notional })
    }
}

impl IbkrClient {
    /// 处理下单响应，包括 reply 确认循环 (最多 5 轮)
    async fn handle_order_response(
        &self,
        initial_resp: &serde_json::Value,
    ) -> Result<OrderId, ExchangeError> {
        let mut current_resp = initial_resp.clone();
        let max_replies = 5;

        for _ in 0..max_replies {
            if let Some(arr) = current_resp.as_array() {
                if let Some(first) = arr.first() {
                    // 情况 1: 直接返回 order_id
                    if let Some(order_id) = first.get("order_id").and_then(|v| v.as_str()) {
                        return Ok(order_id.to_string());
                    }

                    // 情况 2: 需要 reply 确认
                    if let Some(reply_id) = first.get("id").and_then(|v| v.as_str()) {
                        let reply_url = format!(
                            "{}iserver/reply/{}",
                            self.auth.base_url(),
                            reply_id
                        );

                        let resp = self
                            .authed_request("POST", &reply_url)?
                            .json(&serde_json::json!({"confirmed": true}))
                            .send()
                            .await
                            .map_err(Self::map_reqwest_error)?;

                        current_resp =
                            resp.json().await.map_err(Self::map_reqwest_error)?;
                        continue;
                    }
                }
            }

            return Err(ExchangeError::OrderRejected(
                Exchange::IBKR,
                format!("Unexpected order response: {}", current_resp),
            ));
        }

        Err(ExchangeError::OrderRejected(
            Exchange::IBKR,
            "Too many reply confirmations".to_string(),
        ))
    }
}

/// 从 IBKR account summary 中提取数值字段
///
/// 尝试多个候选字段名，支持 `{"amount": f64}` 嵌套格式、直接 f64、字符串 f64
fn extract_summary_value(summary: &serde_json::Value, field_names: &[&str]) -> Option<f64> {
    for name in field_names {
        if let Some(v) = summary.get(name) {
            if let Some(amount) = v.get("amount").and_then(|a| a.as_f64()) {
                return Some(amount);
            }
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

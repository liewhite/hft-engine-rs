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
        let resp = auth.authed_request(&http, "POST", &init_url)
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?
            .json(&serde_json::json!({"publish": true, "compete": true}))
            .send()
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?;

        tracing::info!(status = %resp.status(), "IBKR brokerage session init");

        // 3. 获取 account_id (GET /portfolio/accounts)
        let accounts_url = format!("{}portfolio/accounts", base_url);
        let resp = auth.authed_request(&http, "GET", &accounts_url)
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?
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
        let resp = auth.authed_request(&http, "POST", &switch_url)
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?
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
        match auth.authed_request(&http, "POST", &suppress_url)
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))?
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

    /// 获取持仓列表
    ///
    /// 调用 GET /portfolio/{accountId}/positions/0
    /// 返回 Vec<Position>，仅包含已配置的 symbol
    pub async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        let base_url = self.auth.base_url();

        // 先预热 portfolio accounts 缓存
        let recv_url = format!("{}portfolio/accounts", base_url);
        if let Err(e) = self.authed_request("GET", &recv_url)?.send().await {
            tracing::warn!(error = %e, "IBKR portfolio/accounts prefetch failed");
        }

        // 使用 portfolio2 接口: 近实时数据，无缓存
        // (portfolio v1 的 /positions/{page} 有缓存延迟，invalidate 也不可靠)
        let url = format!(
            "{}portfolio2/{}/positions",
            base_url, self.account_id
        );

        let resp = self
            .authed_request("GET", &url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let body: serde_json::Value = resp.json().await.map_err(Self::map_reqwest_error)?;

        // 构建 conid → symbol 反向映射
        let conid_to_symbol: HashMap<i64, &str> = self
            .conids
            .iter()
            .map(|(symbol, &conid)| (conid, symbol.as_str()))
            .collect();

        let arr = match body.as_array() {
            Some(arr) => arr,
            None => {
                tracing::warn!(body = %body, "IBKR positions response is not an array");
                return Ok(Vec::new());
            }
        };

        let mut positions = Vec::new();
        for item in arr {
            // portfolio2 返回 conid 为字符串，portfolio v1 为数字，兼容两种格式
            let conid = match item.get("conid") {
                Some(v) => {
                    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                }
                None => None,
            };
            let conid = match conid {
                Some(c) => c,
                None => {
                    tracing::warn!(item = %item, "IBKR position missing/invalid conid");
                    continue;
                }
            };

            // 跳过未配置的 symbol（不记录 warn，IBKR 账户可能持有其他股票）
            let symbol = match conid_to_symbol.get(&conid) {
                Some(s) => s,
                None => continue,
            };

            let size = match item.get("position").and_then(|v| v.as_f64()) {
                Some(s) => s,
                None => {
                    tracing::warn!(item = %item, "IBKR position missing/invalid size");
                    continue;
                }
            };

            let avg_cost = item
                .get("avgCost")
                .and_then(|v| v.as_f64())
                .unwrap_or_else(|| {
                    tracing::warn!(symbol = %symbol, item = %item, "IBKR position missing/invalid avgCost");
                    0.0
                });
            let unrealized_pnl = item
                .get("unrealizedPnl")
                .and_then(|v| v.as_f64())
                .unwrap_or_else(|| {
                    tracing::warn!(symbol = %symbol, item = %item, "IBKR position missing/invalid unrealizedPnl");
                    0.0
                });

            positions.push(crate::domain::Position {
                exchange: Exchange::IBKR,
                symbol: symbol.to_string(),
                size,
                entry_price: avg_cost,
                unrealized_pnl,
            });
        }

        tracing::debug!(count = positions.len(), "IBKR positions fetched");
        Ok(positions)
    }

    /// 查询 AAPL 交易时间表
    ///
    /// 调用 GET /trsrv/secdef/schedule?assetClass=STK&symbol=AAPL
    /// 返回交易时段列表，用于判断当前市场状态
    pub async fn fetch_trading_schedule(&self) -> Result<Vec<TradingSchedule>, ExchangeError> {
        let url = format!(
            "{}trsrv/secdef/schedule?assetClass=STK&symbol=AAPL",
            self.auth.base_url()
        );

        let resp = self
            .authed_request("GET", &url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let body: serde_json::Value = resp.json().await.map_err(Self::map_reqwest_error)?;

        // 防御性解析：响应可能是数组或单个对象
        let items = if let Some(arr) = body.as_array() {
            arr.clone()
        } else if body.is_object() {
            vec![body]
        } else {
            tracing::warn!(body = %body, "IBKR schedule response unexpected format");
            return Ok(Vec::new());
        };

        let mut schedules = Vec::new();
        for item in &items {
            let id = item.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let trade_venue_id = item.get("tradeVenueId").and_then(|v| v.as_str()).map(|s| s.to_string());
            let description = item.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());

            let entry_schedules = match item.get("schedules").and_then(|v| v.as_array()) {
                Some(arr) => {
                    arr.iter()
                        .map(|entry| {
                            // IBKR API: 交易时段在 "tradingtimes" 字段 (非 "sessions")
                            let sessions = entry
                                .get("tradingtimes")
                                .and_then(|v| v.as_array())
                                .map(|sarr| {
                                    sarr.iter()
                                        .map(|s| TradingSession {
                                            opening_time: s.get("openingTime").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                            closing_time: s.get("closingTime").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                            prop: s.get("prop").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();

                            ScheduleEntry {
                                // 统一去掉连字符，兼容 "2000-01-03" 和 "20000103" 两种格式
                                trading_schedule_date: entry.get("tradingScheduleDate").and_then(|v| v.as_str()).map(|s| s.replace('-', "")),
                                sessions,
                            }
                        })
                        .collect()
                }
                None => {
                    tracing::warn!(item = %item, "IBKR schedule item missing 'schedules' array");
                    Vec::new()
                }
            };

            schedules.push(TradingSchedule {
                id,
                trade_venue_id,
                description,
                schedules: entry_schedules,
            });
        }

        Ok(schedules)
    }

    /// 构建带认证 header 的请求
    fn authed_request(
        &self,
        method: &str,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, ExchangeError> {
        self.auth
            .authed_request(&self.http, method, url)
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::IBKR, e.to_string()))
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
            "cOID": order.client_order_id,
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

/// IBKR snapshot field tag: Bid Price
const SNAPSHOT_FIELD_BID: &str = "84";
/// IBKR snapshot field tag: Ask Price
const SNAPSHOT_FIELD_ASK: &str = "86";

impl IbkrClient {
    /// 获取指定 symbol 的 snapshot BBO (bid, ask)
    ///
    /// IBKR 股票无公开 BBO REST API，通过 `/iserver/marketdata/snapshot` 获取。
    /// 首次请求可能触发订阅，需要多次请求才能拿到数据。
    pub async fn fetch_snapshot_bbo(&self, symbol: &str) -> Result<(f64, f64), ExchangeError> {
        let conid = self.conids.get(symbol).ok_or_else(|| {
            ExchangeError::SymbolNotFound(Exchange::IBKR, symbol.to_string())
        })?;

        let url = format!(
            "{}iserver/marketdata/snapshot?conids={}&fields={},{}",
            self.auth.base_url(),
            conid,
            SNAPSHOT_FIELD_BID,
            SNAPSHOT_FIELD_ASK
        );

        for attempt in 0..3u8 {
            let resp = self
                .authed_request("GET", &url)?
                .send()
                .await
                .map_err(Self::map_reqwest_error)?;

            let body: serde_json::Value = resp.json().await.map_err(Self::map_reqwest_error)?;

            let arr = body.as_array().ok_or_else(|| {
                ExchangeError::ConnectionFailed(
                    Exchange::IBKR,
                    format!("snapshot 响应不是数组: {}", body),
                )
            })?;

            let first = arr.first().ok_or_else(|| {
                ExchangeError::ConnectionFailed(
                    Exchange::IBKR,
                    "snapshot 响应数组为空".to_string(),
                )
            })?;

            let bid = parse_snapshot_field(first, SNAPSHOT_FIELD_BID);
            let ask = parse_snapshot_field(first, SNAPSHOT_FIELD_ASK);

            if let (Some(b), Some(a)) = (bid, ask) {
                tracing::debug!(attempt, bid = b, ask = a, "IBKR snapshot");
                return Ok((b, a));
            }

            // 字段缺失 = 数据未就绪，等待重试
            tracing::debug!(attempt, "IBKR snapshot 数据未就绪");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(ExchangeError::ConnectionFailed(
            Exchange::IBKR,
            format!("3 次尝试后仍无法获取 {} 的 snapshot 价格", symbol),
        ))
    }

    /// 获取指定 symbol 的 snapshot 中间价
    pub async fn fetch_snapshot_mid_price(&self, symbol: &str) -> Result<f64, ExchangeError> {
        let (bid, ask) = self.fetch_snapshot_bbo(symbol).await?;
        Ok((bid + ask) / 2.0)
    }

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

    /// 查询当天 live orders (原始 JSON)
    pub async fn fetch_live_orders(&self) -> Result<serde_json::Value, ExchangeError> {
        let url = format!("{}iserver/account/orders", self.auth.base_url());
        let resp = self
            .authed_request("GET", &url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        resp.json().await.map_err(Self::map_reqwest_error)
    }

    /// 使 IBKR 持仓缓存失效
    pub async fn invalidate_positions_cache(&self) {
        let url = format!(
            "{}portfolio/{}/positions/invalidate",
            self.auth.base_url(),
            self.account_id
        );
        match self.authed_request("POST", &url) {
            Ok(req) => {
                if let Err(e) = req.send().await {
                    tracing::warn!(error = %e, "IBKR invalidate positions cache failed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "IBKR invalidate positions cache request build failed");
            }
        }
    }
}

/// 解析 snapshot 响应中的价格字段
///
/// 字段不存在时返回 None（数据未就绪），字段存在但格式异常时 panic（API 不兼容）。
fn parse_snapshot_field(data: &serde_json::Value, field: &str) -> Option<f64> {
    let v = data.get(field)?;
    Some(v.as_f64().unwrap_or_else(|| {
        v.as_str()
            .unwrap_or_else(|| panic!("snapshot field {} 既不是 f64 也不是字符串: {}", field, v))
            .parse()
            .unwrap_or_else(|_| panic!("snapshot field {} 字符串无法解析为 f64: {}", field, v))
    }))
}

// ============================================================================
// IBKR Trading Schedule 数据结构
// ============================================================================

/// IBKR 交易时间表
#[derive(Debug)]
pub struct TradingSchedule {
    pub id: Option<String>,
    pub trade_venue_id: Option<String>,
    pub description: Option<String>,
    pub schedules: Vec<ScheduleEntry>,
}

/// 交易日程条目
#[derive(Debug)]
pub struct ScheduleEntry {
    /// 交易日期 (格式: "YYYYMMDD"，已去连字符标准化)
    /// - 周几模式: "20000101"=Sat, "20000103"=Mon, ..., "20000107"=Fri
    /// - 精确日期: 节假日用实际日期 (如 "20260403" = Good Friday)
    pub trading_schedule_date: Option<String>,
    /// 交易时段列表
    pub sessions: Vec<TradingSession>,
}

/// 单个交易时段
#[derive(Debug)]
pub struct TradingSession {
    /// 开盘时间 (格式: "HHmm"，如 "0930")
    pub opening_time: Option<String>,
    /// 收盘时间 (格式: "HHmm"，如 "1600")
    pub closing_time: Option<String>,
    /// 时段属性 (如 "LIQUID", "PRE-OPEN" 等)
    pub prop: Option<String>,
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

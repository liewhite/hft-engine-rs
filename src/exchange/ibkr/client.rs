//! IBKR ExchangeClient 实现 (仅 REST)

use crate::domain::{
    Exchange, ExchangeError, OrderId, OrderStatus, OrderType, Side, Symbol, SymbolMeta,
    TimeInForce,
};
use crate::exchange::client::{ExchangeClient, ExchangeOrder};
use crate::exchange::ibkr::auth::IbkrAuth;
use crate::exchange::ibkr::symbol::resolve_conids;
use crate::exchange::ibkr::wire;
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

    /// 查询所有持仓（内部实现；ExchangeClient::fetch_positions 委托到这里）
    ///
    /// 调用 GET /portfolio2/{accountId}/positions
    /// 返回 Vec<Position>，仅包含已配置的 symbol
    async fn fetch_positions_impl(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
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

        // 非数组即**报错**，不能退化成空列表。IBKR CPAPI 会以 200 返回 `{"error": ...}`
        // 这类响应体；而本方法是持仓基线的唯一来源（见 ExchangeClient::fetch_positions 的
        // 契约：`Ok(vec![])` 只意味着账户确实空仓），静默返回空会让基线变成全零、此后由 Fill
        // 累加且永不纠正 —— 与 Hyperliquid 那个"过滤全空即报错"守卫防的是同一类静默错账。
        let arr = body.as_array().ok_or_else(|| {
            let preview = body.to_string();
            ExchangeError::ParseError(format!(
                "IBKR positions 响应不是数组（持仓基线不能以空列表蒙混过去）: {}",
                &preview[..preview.len().min(500)]
            ))
        })?;

        let mut positions = Vec::new();
        for item in arr {
            // portfolio2 返回 conid 为字符串，portfolio v1 为数字，兼容两种格式
            let conid = item
                .get("conid")
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
            // conid 认不出 = 这条持仓归属不明。**不能跳过**：跳过等于把它当成"该
            // symbol 空仓"，而这份读数既是基线（写一次、永不纠正）又是对账读数
            // （两条通道共用本函数）—— 字段名/类型一变，基线 0、读数也 0，对账判
            // "一致"，漂移检测结构性失明。与 Hyperliquid 的"过滤全空即报错"同口径。
            let conid = conid.ok_or_else(|| {
                ExchangeError::ParseError(format!(
                    "IBKR 持仓条目的 conid 缺失或无法解析，整份持仓读数不可信: {item}"
                ))
            })?;

            // 跳过未配置的 symbol：这是合法的"不归本引擎管"（IBKR 账户可能持有其他
            // 股票），与上面"认不出 conid"是两回事 —— 那是数据坏了，这是范围之外。
            let symbol = match conid_to_symbol.get(&conid) {
                Some(s) => s,
                None => continue,
            };

            // 双态解析（Number|String，含千分位）—— 与挂单/成交路径同一出处，见 wire 模块。
            // 只按 as_f64() 解析会让字符串形态得到 None：这份读数同时是基线与对账读数，
            // 一个格式差异就能让启动被挡死或对账持续报错。
            let size = item
                .get("position")
                .and_then(wire::number)
                .ok_or_else(|| {
                    ExchangeError::ParseError(format!(
                        "IBKR {symbol} 持仓数量缺失或无法解析，整份持仓读数不可信: {item}"
                    ))
                })?;

            positions.push(crate::domain::Position {
                exchange: Exchange::IBKR,
                symbol: symbol.to_string(),
                size,
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

    async fn cancel_order(&self, _symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError> {
        // IBKR 按 orderId 撤单，与 symbol 无关（orderId 在账户内唯一）
        let url = format!(
            "{}iserver/account/{}/order/{}",
            self.auth.base_url(),
            self.account_id,
            order_id
        );

        let resp = self
            .authed_request("DELETE", &url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        if !status.is_success() {
            return Err(ExchangeError::ApiError(
                Exchange::IBKR,
                status.as_u16() as i32,
                text,
            ));
        }

        // IBKR 会以 200 返回 `{"error": "..."}`，不展开检查就会把失败当成功 ——
        // 而调用方（启动期/降级撤单）正是靠返回值判断遗留挂单是否已清理。
        if let Ok(body) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(err) = body.get("error").and_then(|e| e.as_str()) {
                return Err(ExchangeError::Other(format!(
                    "IBKR cancel order {order_id} failed: {err}"
                )));
            }
        }
        Ok(())
    }

    async fn fetch_pending_orders(
        &self,
        symbol: &Symbol,
    ) -> Result<Vec<crate::domain::OrderUpdate>, ExchangeError> {
        let Some(&target_conid) = self.conids.get(symbol) else {
            // 未解析出 conid 的 symbol 本就下不了单，自然也不会有挂单
            return Ok(Vec::new());
        };

        #[derive(serde::Deserialize)]
        struct Response {
            #[serde(default)]
            orders: Vec<LiveOrder>,
        }

        /// `GET /iserver/account/orders` 的条目。
        ///
        /// 字段一律 `Option` / `default`：IBKR 对不同订单类型返回的字段集并不齐整，
        /// 缺字段是常态而非异常，不该让整条响应解析失败 —— 那会把启动挡在门外。
        /// 真正必需的只有 `orderId`（撤单要用）与 `conid`（定位 symbol）。
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LiveOrder {
            #[serde(default)]
            order_id: Option<serde_json::Value>,
            /// 下单时带上的 client_order_id：请求里叫 `cOID`，而**响应里 IBKR 用
            /// `order_ref` 回传**（本仓库 WS 侧两个处理器都是取 order_ref，见
            /// `actor/public_ws.rs`）。两个名字都接，缺一就会让归属识别全线失效：
            /// `own_pending_orders` 认不出自己的单 → 启动期"撤净遗留挂单"谎报成功、
            /// 撤单复查因 client_order_id 全为 None 而无法核实。
            #[serde(rename = "cOID", alias = "order_ref", default)]
            coid: Option<String>,
            #[serde(default)]
            conid: Option<i64>,
            #[serde(default)]
            side: Option<String>,
            #[serde(default)]
            status: Option<String>,
            // 用 `Value` 而非 `Option<f64>`：`#[serde(default)]` 只兜"字段缺失"，
            // **类型不符照样让整条响应反序列化失败** —— IBKR 把价量以字符串
            // `"155.69"` 下发时，本方法会返回 ParseError，而它是启动期撤遗留挂单的
            // 唯一数据源，于是整机启动被一个格式差异挡死。数字与字符串两种形态统一
            // 交给 `wire::optional_number` 判定（见该模块文档）。
            #[serde(default)]
            total_size: Option<serde_json::Value>,
            #[serde(default)]
            filled_quantity: Option<serde_json::Value>,
        }

        /// 把 LiveOrder 的双态数值字段取成 f64；字段缺失按 `missing` 兜底（那是"交易所
        /// 没给"，对挂单快照的用途无害：撤单只需要 order_id），**解析失败则报错**。
        fn order_number(
            raw: &Option<serde_json::Value>,
            field: &str,
            missing: f64,
        ) -> Result<f64, ExchangeError> {
            match raw {
                None | Some(serde_json::Value::Null) => Ok(missing),
                Some(v) => crate::exchange::ibkr::wire::number(v).ok_or_else(|| {
                    ExchangeError::ParseError(format!(
                        "IBKR live order 字段 {field} 无法解析为数字: {v}"
                    ))
                }),
            }
        }

        let url = format!("{}iserver/account/orders", self.auth.base_url());
        let resp = self
            .authed_request("GET", &url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        if !status.is_success() {
            return Err(ExchangeError::ApiError(
                Exchange::IBKR,
                status.as_u16() as i32,
                text,
            ));
        }
        let body: Response = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::ParseError(format!(
                "解析 IBKR live orders 失败: {e}; 响应: {}",
                &text[..text.len().min(500)]
            ))
        })?;

        let mut updates = Vec::new();
        for o in body.orders {
            if o.conid != Some(target_conid) {
                continue;
            }
            // 只保留**未终结**的单：本方法的用途是"还挂在簿上的是哪些"
            let raw_status = o.status.as_deref().unwrap_or_default();
            let filled = order_number(&o.filled_quantity, "filledQuantity", 0.0)?;
            // 终态**显式枚举**，未识别的 status 报错 —— 与下面 side 的处理同一姿态。
            // 通配 `_ => continue` 会把没见过的状态一律当成"已终态"丢弃，而
            // `PendingCancel`（撤单已请求未确认）的单**仍活在簿上**：撤单若被拒它会回到
            // Submitted。把它丢掉正好落进消费方"不在列表里 ⇒ 已终态"的推断：启动期判
            // "遗留单已撤净"放行启动、撤单复查合成 Cancelled 清掉一张可能复活的单的 pending。
            let order_status = match raw_status {
                "Submitted" if filled > 0.0 => OrderStatus::PartiallyFilled,
                // PendingCancel 仍在簿上，按未终结处理
                "PendingSubmit" | "PreSubmitted" | "Submitted" | "PendingCancel" => {
                    OrderStatus::Pending
                }
                "Filled" | "Cancelled" | "Inactive" | "ApiCancelled" | "Expired" | "Rejected" => {
                    continue
                }
                other => {
                    return Err(ExchangeError::ParseError(format!(
                        "IBKR live order 状态无法识别（'{other}'，conid={target_conid}），\
                         无法判断它是否还挂在簿上，整份挂单快照不可信"
                    )));
                }
            };
            // 双态解析收在 wire::order_id 一处（WS 两条路径共用，见该模块文档）。
            // 没有 orderId 就撤不掉它 —— 不能悄悄跳过，否则启动期撤单会"复查通过"
            // 但实际漏了一张。
            let order_id = wire::order_id(o.order_id.as_ref()).ok_or_else(|| {
                ExchangeError::ParseError(format!(
                    "IBKR live order 缺少 orderId，无法撤单: conid={target_conid} status={raw_status}"
                ))
            })?;
            // 方向认不出**不能跳过**：这份列表的消费方在做「不在列表里 ⇒ 该单已终态」
            // 的推断（启动期判"遗留单已撤净"、撤单复查合成 Cancelled 清本地 pending），
            // 跳过一条等于谎报该单不存在 —— 与 OKX 挂单快照同一个模式，同样必须整份报错。
            let side = match o.side.as_deref().unwrap_or_default() {
                "BUY" | "B" => Side::Long,
                "SELL" | "S" => Side::Short,
                other => {
                    return Err(ExchangeError::ParseError(format!(
                        "IBKR live order 方向无法识别（'{other}'，orderId={order_id}），\
                         整份挂单快照不可信"
                    )));
                }
            };
            updates.push(crate::domain::OrderUpdate {
                order_id,
                client_order_id: o.coid,
                exchange: Exchange::IBKR,
                symbol: symbol.clone(),
                side,
                status: order_status,
                // 缺失按 0：挂单快照的用途是"这张单还在吗 + 怎么撤",价量不参与
                // 判定（本地 pending 重建对 price<=0 有专门守卫，见 messaging::state）
                // IBKR live orders 不含 reduce-only 信息
                quantity: order_number(&o.total_size, "totalSize", 0.0)?,
                // 快照没有"本次成交量"这一概念
            });
        }

        Ok(updates)
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
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
                    // IBKR Web API 没有 post-only 语义。此前静默映射成 GTC —— 那会把
                    // "绝不吃单"的 maker 单变成可能立即成交的吃单，成交行为被悄悄改变
                    // （其余三所都有对应值：GTX / post_only / Alo）。显式拒绝，让策略
                    // 作者在选型时就知道该所不支持，而不是上线后从成交明细里发现。
                    TimeInForce::PostOnly => {
                        return Err(ExchangeError::Other(
                            "IBKR 不支持 post-only（Web API 无对应 tif），拒绝把它静默降级为 GTC"
                                .to_string(),
                        ))
                    }
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

    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
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

        // 两个字段都**必须**取到，取不到即报错，绝不兜 0：
        //
        // - `notional = 0` 是**危险侧**的谎报：它直接喂给账户杠杆闸门
        //   （notional / equity），兜 0 会让 `max_account_leverage` 恒定通过，
        //   无论真实杠杆多高 —— 风控静默失效，而日志里只有一行 warn。
        // - `equity = 0` 看似安全（策略判净值不足会停手），但两个字段是独立解析的：
        //   只要 IBKR 单独改掉 grosspositionvalue 的字段名（本就写了两个候选名，
        //   正因为它变过），就会出现"equity 正常、闸门失效"的组合。
        //
        // 调用方 account_polling 有 Err 分支，报错不会静默丢失。
        let equity = extract_summary_value(&summary, &["netliquidation", "netLiquidationValue"])
            .ok_or_else(|| {
                ExchangeError::ParseError(format!(
                    "IBKR 账户摘要缺 equity（netliquidation/netLiquidationValue）: {summary}"
                ))
            })?;

        let notional = extract_summary_value(&summary, &["grosspositionvalue", "securitiesGVP"])
            .ok_or_else(|| {
                ExchangeError::ParseError(format!(
                    "IBKR 账户摘要缺 notional（grosspositionvalue/securitiesGVP）—— \
                     兜 0 会让账户杠杆闸门恒定通过: {summary}"
                ))
            })?
            .abs();

        Ok(crate::domain::AccountInfo { equity, notional })
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        self.fetch_positions_impl().await
    }
}

/// IBKR snapshot field tag: Bid Price
const SNAPSHOT_FIELD_BID: &str = "84";
/// IBKR snapshot field tag: Ask Price
const SNAPSHOT_FIELD_ASK: &str = "86";

/// 构造 `/iserver/marketdata/snapshot` 的请求 URL（`base_url` 以 `/` 结尾）。
///
/// snapshot 与行情预热、ws 订阅共用同一 endpoint 与同一套字段 tag，故 URL 拼装收在此一处
/// （单一数据源），避免各调用点各写一份 format!。
/// snapshot 响应是否已带齐调用方声明的必需字段。
///
/// `required` 为空 = 任何响应都算就绪（诊断用途：拿到什么打什么）。
fn snapshot_ready(obj: &serde_json::Value, required: &[&str]) -> bool {
    required.iter().all(|f| obj.get(*f).is_some())
}

fn snapshot_url(base_url: &str, conid: i64, fields: &[&str]) -> String {
    format!(
        "{}iserver/marketdata/snapshot?conids={}&fields={}",
        base_url,
        conid,
        fields.join(",")
    )
}

impl IbkrClient {
    /// 获取指定 symbol 的 snapshot BBO (bid, ask)
    ///
    /// IBKR 股票无公开 BBO REST API，通过 `/iserver/marketdata/snapshot` 获取。
    /// 首次请求可能触发订阅，需要多次请求才能拿到数据。
    pub async fn fetch_snapshot_bbo(&self, symbol: &str) -> Result<(f64, f64), ExchangeError> {
        let conid = self.conids.get(symbol).ok_or_else(|| {
            ExchangeError::SymbolNotFound(Exchange::IBKR, symbol.to_string())
        })?;

        let url = snapshot_url(
            self.auth.base_url(),
            *conid,
            &[SNAPSHOT_FIELD_BID, SNAPSHOT_FIELD_ASK],
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

    /// 通用 snapshot：按 conid + 字段 tag 拉 `/iserver/marketdata/snapshot`，返回首个对象的原始 JSON。
    ///
    /// 字段 tag 与 conid 全由调用方给出（本方法不内置任何业务字段语义），故可用于借券费/
    /// 可借量/外汇等任意字段——调用方自行解析。这是 IbkrClient 上的**具体方法**，不进
    /// ExchangeClient trait。
    ///
    /// `required`：**由调用方声明哪些字段缺了就得重试**（IBKR 首次请求常只回订阅空壳，字段要
    /// 下一轮才附上）。空切片 = 拿到任何响应即返回（诊断用途）。
    ///
    /// 为什么必须由调用方声明：本方法原先用"任意一个请求字段有值"当就绪判据，而 IBKR **总会**
    /// 返回 `6509`（行情可用性），于是只要请求里带了 6509，判据恒真、重试形同虚设——空壳被当成
    /// 拿到，调用方再报"字段缺失"。就绪与否是业务语义（借券腿要的是费率，冻结态可借量本就缺），
    /// 不该由 client 猜。
    pub async fn fetch_snapshot_raw(
        &self,
        conid: i64,
        fields: &[&str],
        required: &[&str],
    ) -> Result<serde_json::Value, ExchangeError> {
        let url = snapshot_url(self.auth.base_url(), conid, fields);

        for attempt in 0..3u8 {
            let resp = self
                .authed_request("GET", &url)?
                .send()
                .await
                .map_err(Self::map_reqwest_error)?;
            let body: serde_json::Value = resp.json().await.map_err(Self::map_reqwest_error)?;

            let first = body
                .as_array()
                .and_then(|arr| arr.first())
                .cloned();

            if let Some(obj) = first {
                if snapshot_ready(&obj, required) {
                    return Ok(obj);
                }
            }
            tracing::debug!(attempt, conid, ?required, "IBKR snapshot 必需字段未就绪，重试");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(ExchangeError::ConnectionFailed(
            Exchange::IBKR,
            format!(
                "3 次尝试后 conid={} 的 snapshot 仍缺必需字段 {:?} (请求字段 {:?})",
                conid, required, fields
            ),
        ))
    }

    /// 行情预热 (pre-flight)：对 conid 打一次 `/iserver/marketdata/snapshot`，让 IServer 开始
    /// 消费该合约的实时数据流。
    ///
    /// IBKR 要求这个前置请求：未预热的 conid，ws `smd` 只会回一份订阅瞬间的值、之后不再推增量
    /// （实测 KRX 腿因此每 2~3 小时才更新一次，而同会话内被 snapshot 轮询"顺带预热"的美股腿
    /// 一直是毫秒级推送）。故 `smd` 订阅前必须先调本方法。
    ///
    /// 按 IBKR 文档，预热请求**本身不保证返回数据**，其价值在于服务端副作用，故只判 HTTP 状态、
    /// 不看 body：预热成功 ⇔ IServer 接受了这次请求。要"拿到值"请用 `fetch_snapshot_raw`。
    /// 失败即数据源不可用，交由调用方致命处理（不在此重试、不降级）。
    pub async fn preflight_market_data(
        &self,
        conid: i64,
        fields: &[&str],
    ) -> Result<(), ExchangeError> {
        let url = snapshot_url(self.auth.base_url(), conid, fields);
        self.authed_request("GET", &url)?
            .send()
            .await
            .map_err(Self::map_reqwest_error)?
            .error_for_status()
            .map_err(Self::map_reqwest_error)?;
        Ok(())
    }

    /// 直读现汇参考汇率：GET `/iserver/exchangerate?target={target}&source={source}`，返回 `rate`。
    ///
    /// 与 snapshot 的 last(31) 不同，这是**专用汇率端点**：不依赖成交价，盘外/冻结态亦返回参考
    /// 汇率——受限货币（如 KRW）休市时 last 为空、bid/ask 根本不返回，只有此端点可靠。
    /// `rate` 语义为「1 单位 source = rate 单位 target」（如 source=USD,target=KRW → 1 USD = rate KRW）。
    /// 首次请求可能未就绪，故重试。签名口径与 `fetch_snapshot_raw` 一致（query 参与 OAuth 签名）。
    pub async fn fetch_exchange_rate(
        &self,
        target: &str,
        source: &str,
    ) -> Result<f64, ExchangeError> {
        let url = format!(
            "{}iserver/exchangerate?target={}&source={}",
            self.auth.base_url(),
            target,
            source
        );

        for attempt in 0..3u8 {
            let resp = self
                .authed_request("GET", &url)?
                .send()
                .await
                .map_err(Self::map_reqwest_error)?;
            let body: serde_json::Value = resp.json().await.map_err(Self::map_reqwest_error)?;

            if let Some(rate) = body.get("rate").and_then(|v| v.as_f64()) {
                return Ok(rate);
            }
            tracing::debug!(attempt, target, source, body = %body, "IBKR exchangerate 未就绪，重试");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(ExchangeError::ConnectionFailed(
            Exchange::IBKR,
            format!("3 次尝试后仍无法获取 {source}/{target} 汇率"),
        ))
    }

    /// 按 symbol 解析 conid（用于调用方拿配置里的 symbol 对应 conid；forex 等不在表内的直接用 conid）
    pub fn conid_of(&self, symbol: &str) -> Option<i64> {
        self.conids.get(symbol).copied()
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
                crate::domain::RejectReason::classify(&format!(
                    "Unexpected order response: {}",
                    current_resp
                )),
            ));
        }

        Err(ExchangeError::OrderRejected(
            Exchange::IBKR,
            crate::domain::RejectReason::Other("Too many reply confirmations".to_string()),
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
/// 字段不存在时返回 None（数据未就绪）。字段存在但格式异常（既非 f64、
/// 也非可解析字符串）时同样返回 None 并 warn，视为该字段数据未就绪，
/// 不 panic（避免因单个字段格式波动打崩进程）。
fn parse_snapshot_field(data: &serde_json::Value, field: &str) -> Option<f64> {
    let v = data.get(field)?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    match v.as_str().and_then(|s| s.parse::<f64>().ok()) {
        Some(n) => Some(n),
        None => {
            tracing::warn!(field = %field, value = %v, "IBKR snapshot 字段格式异常，视为未就绪");
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_url_joins_fields_with_comma() {
        assert_eq!(
            snapshot_url("https://api.ibkr.com/v1/api/", 17382246, &["84", "86", "6509"]),
            "https://api.ibkr.com/v1/api/iserver/marketdata/snapshot?conids=17382246&fields=84,86,6509"
        );
    }

    #[test]
    fn snapshot_ready_requires_all_declared_fields() {
        let obj = serde_json::json!({"6509": "RpB", "84": "155.69", "86": "155.90"});
        // 回归本次 bug：请求带 6509 时，旧判据 (any) 恒真 → 空壳被当成拿到、重试形同虚设
        assert!(
            !snapshot_ready(&obj, &["7637"]),
            "缺必需字段必须判未就绪，即使响应里有 6509/BBO 字段"
        );
        assert!(!snapshot_ready(&obj, &["7636", "7637"]));
        assert!(snapshot_ready(&obj, &["84", "86"]), "必需字段齐 → 就绪");
    }

    #[test]
    fn snapshot_ready_with_no_required_accepts_anything() {
        let obj = serde_json::json!({"conid": 899700992});
        assert!(
            snapshot_ready(&obj, &[]),
            "诊断用途：不声明必需字段则任何响应都算就绪"
        );
    }

    #[test]
    fn snapshot_url_single_field() {
        assert_eq!(
            snapshot_url("https://x/", 1, &["31"]),
            "https://x/iserver/marketdata/snapshot?conids=1&fields=31"
        );
    }
}

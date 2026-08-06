//! Binance ExchangeClient 实现 (仅 REST)

use super::symbol::{from_binance, to_binance};
use crate::domain::{
    Exchange, ExchangeError, FundingFee, OrderId, OrderStatus, OrderType, RejectReason, Side,
    Symbol, SymbolMeta, TimeInForce, Timestamp,
};
pub use crate::exchange::binance::BinanceCredentials;
use crate::exchange::binance::REST_BASE_URL;
use crate::exchange::client::{ExchangeClient, ExchangeOrder};
use crate::exchange::utils::StepFormatter;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
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
    /// 计价币种 (e.g., "USDT")
    quote: String,
}

impl BinanceClient {
    /// 创建新的 Binance 客户端
    pub fn new(quote: String, credentials: Option<BinanceCredentials>) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

        Ok(Self {
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
            quote,
        })
    }

    /// 拉取各交易对最近 24 小时的**计价币成交额**（USDT 名义额）。
    ///
    /// 用于按流动性挑选标的：低流动性 symbol 的盘口很差，在其上跑出的模拟结论本身不可信。
    ///
    /// 返回的是**内部基础符号**，且只含当前 quote 的交易对；调用方通常还要与
    /// [`ExchangeClient::fetch_all_symbol_metas`] 取交集，以排除不可交易的标的
    /// （本接口按 24h 行情返回，含已停牌/交割品种）。
    pub async fn fetch_quote_volumes(&self) -> Result<Vec<(Symbol, f64)>, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Ticker24h {
            symbol: String,
            /// 24h 计价币成交额
            quote_volume: String,
        }

        let url = format!("{}/fapi/v1/ticker/24hr", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        if !status.is_success() {
            return Err(ExchangeError::Other(format!(
                "ticker/24hr failed: {status}, body: {}",
                &text[..text.len().min(500)]
            )));
        }
        let tickers: Vec<Ticker24h> = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::Other(format!("Failed to parse ticker/24hr: {e}"))
        })?;

        Ok(tickers
            .into_iter()
            .filter_map(|t| {
                let symbol = from_binance(&t.symbol, &self.quote)?;
                let volume = t.quote_volume.parse::<f64>().ok()?;
                Some((symbol, volume))
            })
            .collect())
    }

    /// 获取计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 获取凭证（供 ManagerActor 创建 BinanceActor 使用）
    pub fn credentials(&self) -> Option<&BinanceCredentials> {
        self.credentials.as_ref()
    }

    /// 获取 REST API 基础 URL
    pub fn rest_base_url(&self) -> &str {
        &self.base_url
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

    /// 获取所有交易对信息
    async fn get_all_exchange_info(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        #[derive(Deserialize)]
        struct ExchangeInfo {
            symbols: Vec<SymbolInfo>,
        }

        #[derive(Deserialize,Debug)]
        #[serde(rename_all = "camelCase")]
        struct SymbolInfo {
            symbol: String,
            filters: Vec<Filter>,
        }

        #[derive(Deserialize,Debug)]
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

        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                status.as_u16() as i32,
                text,
            )));
        }

        let info: ExchangeInfo = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::Other(format!(
                "Failed to parse exchangeInfo: {}, response preview: {}",
                e,
                &text[..text.len().min(500)]
            ))
        })?;

        // 剔除必须留痕（与 OKX 同口径，见那边的说明）：被剔掉的 symbol 会一路静默失效，
        // 而三种症状（下单被拒 / 订阅被忽略 / 回报无法折算）看不出共同的根因。
        // 这里只记录不报错，真正的拦截在投产期 `validate_subscriptions`。
        let mut metas = Vec::new();
        for s in info.symbols {
            let Some(symbol) = from_binance(&s.symbol, &self.quote) else {
                continue; // 非当前 quote 的交易对，不属于本引擎的宇宙
            };
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

            let positive = |v: Option<f64>| v.filter(|&v| v > 0.0);
            match (
                positive(price_step),
                positive(size_step),
                positive(min_order_size),
            ) {
                (Some(price_step), Some(size_step), Some(min_order_size)) => {
                    metas.push(SymbolMeta {
                        exchange: Exchange::Binance,
                        symbol,
                        price_formatter: Arc::new(StepFormatter::new(price_step)),
                        size_step,
                        min_order_size,
                        contract_size: 1.0,
                    });
                }
                _ => tracing::error!(
                    binance_symbol = %s.symbol, %symbol,
                    ?price_step, ?size_step, ?min_order_size,
                    "Binance 合约过滤器缺少有效的价格/数量步长，该 symbol 不可交易"
                ),
            }
        }

        Ok(metas)
    }

    /// 查询所有持仓（内部实现；ExchangeClient::fetch_positions 委托到这里）
    async fn fetch_positions_impl(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        #[allow(dead_code)]
        struct PositionInfo {
            symbol: String,
            position_amt: String,
            entry_price: String,
            mark_price: String, // API 返回但不使用
            un_realized_profit: String,
            leverage: String,
            /// 单向模式恒为 "BOTH"；双向（对冲）模式为 "LONG"/"SHORT"
            position_side: String,
        }

        let query = self
            .build_signed_query(&[])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .get(format!("{}/fapi/v2/positionRisk?{}", self.base_url, query))
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

        let positions: Vec<PositionInfo> = resp.json().await.map_err(Self::map_reqwest_error)?;

        let mut result = Vec::new();
        for p in positions {
            // 双向（对冲）持仓模式下同一 symbol 返回 LONG/SHORT 两行，照单全收会产出
            // 两条同 symbol 的 Position —— 基线与对账全部串账。本系统按单向净持仓建模，
            // 检测到双向模式直接报错拒绝（与 OKX 适配层在 codec 里拒绝 long/short
            // posSide 的立场一致），由使用者去交易所切换持仓模式。
            if p.position_side != "BOTH" {
                return Err(ExchangeError::Other(format!(
                    "Binance 账户处于双向持仓模式（{} positionSide={}），本系统只支持单向\
                     净持仓，请在交易所切换为单向模式",
                    p.symbol, p.position_side
                )));
            }
            let symbol = match from_binance(&p.symbol, &self.quote) {
                Some(s) => s,
                None => continue, // 非当前 quote 的交易对，跳过
            };
            let size: f64 = p.position_amt.parse()
                .map_err(|_| ExchangeError::Other(format!(
                    "Failed to parse position_amt '{}' for {}", p.position_amt, p.symbol)))?;
            // 跳过空仓位
            if size.abs() < 1e-10 {
                continue;
            }
            let entry_price: f64 = p.entry_price.parse()
                .map_err(|_| ExchangeError::Other(format!(
                    "Failed to parse entry_price '{}' for {}", p.entry_price, p.symbol)))?;
            let unrealized_pnl: f64 = p.un_realized_profit.parse()
                .map_err(|_| ExchangeError::Other(format!(
                    "Failed to parse un_realized_profit '{}' for {}", p.un_realized_profit, p.symbol)))?;

            result.push(crate::domain::Position {
                exchange: Exchange::Binance,
                symbol,
                size,
                entry_price,
                unrealized_pnl,
            });
        }

        Ok(result)
    }

    /// 查询资费历史
    ///
    /// 调用 `GET /fapi/v1/income?incomeType=FUNDING_FEE`，返回区间内所有结算明细。
    /// 非当前 quote 币种（如币本位合约）会被自动跳过。
    ///
    /// 同一笔记录的 `tran_id` 在交易所侧唯一，调用方负责去重。
    pub async fn fetch_funding_fees(
        &self,
        start_ms: Timestamp,
        end_ms: Timestamp,
    ) -> Result<Vec<FundingFee>, ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct IncomeEntry {
            symbol: String,
            #[serde(rename = "incomeType")]
            income_type: String,
            income: String,
            asset: String,
            time: u64,
            #[serde(rename = "tranId")]
            tran_id: u64,
        }

        let start_str = start_ms.to_string();
        let end_str = end_ms.to_string();
        let query = self
            .build_signed_query(&[
                ("incomeType", "FUNDING_FEE"),
                ("startTime", &start_str),
                ("endTime", &end_str),
                ("limit", "1000"),
            ])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .get(format!("{}/fapi/v1/income?{}", self.base_url, query))
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

        let entries: Vec<IncomeEntry> = resp.json().await.map_err(Self::map_reqwest_error)?;

        let mut result = Vec::with_capacity(entries.len());
        for e in entries {
            let symbol = match from_binance(&e.symbol, &self.quote) {
                Some(s) => s,
                None => continue, // 跨 quote 合约（如币本位），跳过
            };
            let amount: f64 = e.income.parse().map_err(|_| {
                ExchangeError::Other(format!(
                    "Failed to parse funding income '{}' for {}",
                    e.income, e.symbol
                ))
            })?;
            result.push(FundingFee {
                exchange: Exchange::Binance,
                symbol,
                asset: e.asset,
                amount,
                timestamp: e.time,
                tran_id: e.tran_id,
            });
        }

        Ok(result)
    }

    /// 查询账户信息 (净值 + 总持仓名义价值)
    async fn get_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AccountResponse {
            total_margin_balance: String,
            positions: Vec<PositionInfo>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PositionInfo {
            notional: String,
        }

        let query = self
            .build_signed_query(&[])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .get(format!("{}/fapi/v3/account?{}", self.base_url, query))
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

        let account: AccountResponse = resp.json().await.map_err(Self::map_reqwest_error)?;

        let equity: f64 = account.total_margin_balance.parse().map_err(|_| {
            ExchangeError::ParseError(format!(
                "Binance parse totalMarginBalance: {}",
                account.total_margin_balance
            ))
        })?;

        // 汇总所有持仓的 notional (取绝对值)
        let mut notional: f64 = 0.0;
        for p in &account.positions {
            let value: f64 = p.notional.parse().map_err(|_| {
                ExchangeError::ParseError(format!("Binance parse notional: {}", p.notional))
            })?;
            notional += value.abs();
        }

        Ok(crate::domain::AccountInfo { equity, notional })
    }
}

#[async_trait]
impl ExchangeClient for BinanceClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        self.get_all_exchange_info().await
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.get_all_exchange_info().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all.into_iter().filter(|m| symbol_set.contains(&m.symbol)).collect())
    }

    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        let inst = to_binance(symbol, &self.quote);
        let query = self
            .build_signed_query(&[("symbol", &inst), ("orderId", order_id)])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .delete(format!("{}/fapi/v1/order?{}", self.base_url, query))
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        if !status.is_success() {
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                status.as_u16() as i32,
                text,
            )));
        }
        Ok(())
    }

    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<crate::domain::OrderUpdate>, ExchangeError> {
        // 无凭证 = 只接公共行情，没有账户就没有挂单（同 fetch_positions 口径）
        if self.credentials.is_none() {
            return Ok(Vec::new());
        }
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OpenOrder {
            order_id: i64,
            client_order_id: String,
            side: String,
            status: String,
            price: String,
            orig_qty: String,
            executed_qty: String,
            #[serde(default)]
            reduce_only: bool,
            time: u64,
        }

        let inst = to_binance(symbol, &self.quote);
        let query = self
            .build_signed_query(&[("symbol", &inst)])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = self
            .client
            .get(format!("{}/fapi/v1/openOrders?{}", self.base_url, query))
            .header("X-MBX-APIKEY", api_key)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        if !status.is_success() {
            return Err(self.parse_error(&text).unwrap_or(ExchangeError::ApiError(
                Exchange::Binance,
                status.as_u16() as i32,
                text,
            )));
        }

        let orders: Vec<OpenOrder> = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::ParseError(format!("Failed to parse Binance openOrders: {e}"))
        })?;

        /// 数值字段解析失败即**传播**，不取默认值。
        ///
        /// 静默取 0 会让一张真实挂单看起来是 0 价 0 量；虽然眼下 cancel_leftover_orders 只用
        /// order_id / client_order_id 决策、不读这几个字段，但一旦有代码开始依赖它们，
        /// 假数据是查不出来的（与 Hyperliquid 同一方法里的处理保持一致）。
        fn parse_field(raw: &str, field: &str) -> Result<f64, ExchangeError> {
            raw.parse().map_err(|_| {
                ExchangeError::ParseError(format!("Binance openOrders {field} 非法: {raw}"))
            })
        }

        let mut updates = Vec::new();
        for o in orders {
            let side = match o.side.as_str() {
                "BUY" => Side::Long,
                "SELL" => Side::Short,
                other => {
                    tracing::warn!(side = other, "Binance openOrders 未知方向，跳过");
                    continue;
                }
            };
            let filled = parse_field(&o.executed_qty, "executedQty")?;
            // openOrders 只返回未终结的单，故只有这两种状态；其余按未知跳过而非猜测
            let status = match o.status.as_str() {
                "NEW" => OrderStatus::Pending,
                "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled { filled },
                other => {
                    tracing::warn!(status = other, "Binance openOrders 未预期状态，跳过");
                    continue;
                }
            };
            // Binance USDⓈ-M 的数量本身就是币本位（contract_size = 1），无需折算
            updates.push(crate::domain::OrderUpdate {
                order_id: o.order_id.to_string(),
                client_order_id: (!o.client_order_id.is_empty()).then_some(o.client_order_id),
                exchange: Exchange::Binance,
                symbol: symbol.clone(),
                side,
                status,
                price: parse_field(&o.price, "price")?,
                reduce_only: o.reduce_only,
                quantity: parse_field(&o.orig_qty, "origQty")?,
                filled_quantity: filled,
                // 快照没有"本次成交量"这一概念
                fill_sz: 0.0,
                timestamp: o.time,
            });
        }

        Ok(updates)
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        let symbol = to_binance(&order.symbol, &self.quote);
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

        if !order.client_order_id.is_empty() {
            params.push(("newClientOrderId", &order.client_order_id));
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
            return Err(self.parse_error(&text).unwrap_or_else(|| {
                ExchangeError::OrderRejected(
                    Exchange::Binance,
                    crate::domain::RejectReason::classify(&text),
                )
            }));
        }

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;
        Ok(data.order_id.to_string())
    }

    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        self.get_account_info().await
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        // 无凭证 = 只接公共行情（见 ExchangeAccess），没有账户可查，空仓是事实而非缺数据。
        // 与 OKX / Hyperliquid 口径一致：若在此返回 Err，会让"模拟盘不配凭证"这种正常配置
        // 在 ManagerActor 拉基线时直接启动失败（基线拉取失败是致命的）。
        if self.credentials.is_none() {
            return Ok(Vec::new());
        }
        self.fetch_positions_impl().await
    }
}

/// 错误码映射
fn map_binance_error(code: i32, msg: &str) -> ExchangeError {
    match code {
        -1003 => ExchangeError::RateLimited(Exchange::Binance, Duration::from_secs(60)),
        -2010 | -2019 => ExchangeError::InsufficientBalance(Exchange::Binance, 0.0, 0.0),
        // -2022: "ReduceOnly Order is rejected" —— reduce-only 单因仓位已平被拒
        -2022 => ExchangeError::OrderRejected(Exchange::Binance, RejectReason::ReduceOnlyClosed),
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

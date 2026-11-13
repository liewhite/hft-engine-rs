//! Binance ExchangeClient 实现 (仅 REST)

use super::symbol::{from_binance, to_binance};
use crate::domain::{Decimal, PriceStep};
use crate::domain::{
    Exchange, ExchangeError, FundingFee, OrderId, OrderStatus, OrderType, PlaceError, RejectReason,
    SettledFunding, Side, Symbol, SymbolMeta, TimeInForce, Timestamp,
};
pub use crate::exchange::binance::BinanceCredentials;
use crate::exchange::binance::{
    FUNDING_RATE, IP_WEIGHT, LIMITER, ORDERS_10S, ORDERS_1M, REST_BASE_URL,
};
use crate::exchange::client::{place_send_error, AccountClient, ExchangeClient, ExchangeOrder};
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

/// 解析 `fapi/v1/exchangeInfo`：只收配置计价币的永续，缺任一必需过滤器的品种剔除并记 error。
fn parse_exchange_info(text: &str, quote: &str) -> Result<Vec<SymbolMeta>, String> {
    #[derive(Deserialize)]
    struct ExchangeInfo {
        symbols: Vec<SymbolInfo>,
    }

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "camelCase")]
    struct SymbolInfo {
        symbol: String,
        filters: Vec<Filter>,
    }

    #[derive(Deserialize, Debug)]
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
        #[serde(rename = "MIN_NOTIONAL")]
        MinNotional { notional: String },
        #[serde(other)]
        Other,
    }

    let info: ExchangeInfo = serde_json::from_str(text).map_err(|e| {
        format!(
            "Failed to parse exchangeInfo: {}, response preview: {}",
            e,
            &text[..text.len().min(500)]
        )
    })?;

    // 剔除必须留痕（与 OKX 同口径，见那边的说明）：被剔掉的 symbol 会一路静默失效，
    // 而三种症状（下单被拒 / 订阅被忽略 / 回报无法折算）看不出共同的根因。
    // 这里只记录不报错，真正的拦截在投产期 `validate_subscriptions`。
    let mut metas = Vec::new();
    for s in info.symbols {
        let Some(symbol) = from_binance(&s.symbol, quote) else {
            continue; // 非当前 quote 的交易对，不属于本引擎的宇宙
        };
        let mut price_step: Option<Decimal> = None;
        let mut size_step: Option<Decimal> = None;
        let mut min_order_size: Option<Decimal> = None;
        let mut min_notional: Option<f64> = None;

        for filter in s.filters {
            match filter {
                Filter::PriceFilter { tick_size } => {
                    price_step = crate::domain::parse_decimal(&tick_size).ok();
                }
                Filter::LotSize { step_size, min_qty } => {
                    size_step = crate::domain::parse_decimal(&step_size).ok();
                    min_order_size = crate::domain::parse_decimal(&min_qty).ok();
                }
                Filter::MinNotional { notional } => {
                    min_notional = notional.parse().ok();
                }
                Filter::Other => {}
            }
        }

        let positive = |v: Option<Decimal>| v.filter(|v| *v > Decimal::ZERO);
        match (
            positive(price_step),
            positive(size_step),
            positive(min_order_size),
            min_notional.filter(|&v| v > 0.0),
        ) {
            (Some(price_step), Some(size_step), Some(min_order_size), Some(min_notional)) => {
                metas.push(SymbolMeta {
                    exchange: Exchange::Binance,
                    symbol,
                    price_rule: Arc::new(PriceStep::new(price_step)),
                    size_step,
                    min_order_size,
                    min_notional,
                    contract_size: Decimal::ONE,
                });
            }
            _ => tracing::error!(
                binance_symbol = %s.symbol, %symbol,
                ?price_step, ?size_step, ?min_order_size, ?min_notional,
                "Binance 合约过滤器缺少有效的价格/数量步长或最小下单金额，该 symbol 不可交易"
            ),
        }
    }

    Ok(metas)
}

impl BinanceClient {
    /// 创建新的 Binance 客户端
    pub fn new(
        quote: String,
        credentials: Option<BinanceCredentials>,
    ) -> Result<Self, ExchangeError> {
        let client = crate::exchange::client::exchange_http_client(Exchange::Binance)?;

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
        // 不带 symbol 取全部：权重 40
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(40)]).await?;
        let resp = crate::net::send(self.client.get(&url), admitted)
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
        let tickers: Vec<Ticker24h> = serde_json::from_str(&text)
            .map_err(|e| ExchangeError::Other(format!("Failed to parse ticker/24hr: {e}")))?;

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

    /// 获取凭证（供装配层创建 BinanceNode 使用）
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

        let mut query_parts: Vec<String> =
            params.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
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
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(1)]).await?;
        let resp = crate::net::send(
            self.client
                .get(format!("{}/fapi/v1/exchangeInfo", self.base_url)),
            admitted,
        )
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

        parse_exchange_info(&text, &self.quote).map_err(ExchangeError::Other)
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
            leverage: String,
            /// 单向模式恒为 "BOTH"；双向（对冲）模式为 "LONG"/"SHORT"
            position_side: String,
        }

        let admitted = LIMITER.query(&[IP_WEIGHT.cost(5)]).await?;
        let query = self
            .build_signed_query(&[])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = crate::net::send(
            self.client
                .get(format!("{}/fapi/v2/positionRisk?{}", self.base_url, query))
                .header("X-MBX-APIKEY", api_key),
            admitted,
        )
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
            let size: Decimal = crate::domain::parse_decimal(&p.position_amt).map_err(|_| {
                ExchangeError::Other(format!(
                    "Failed to parse position_amt '{}' for {}",
                    p.position_amt, p.symbol
                ))
            })?;
            // 跳过空仓位
            if size.is_zero() {
                continue;
            }
            result.push(crate::domain::Position {
                exchange: Exchange::Binance,
                symbol,
                size,
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

        let admitted = LIMITER.query(&[IP_WEIGHT.cost(5)]).await?;
        let query = self
            .build_signed_query(&[])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = crate::net::send(
            self.client
                .get(format!("{}/fapi/v3/account?{}", self.base_url, query))
                .header("X-MBX-APIKEY", api_key),
            admitted,
        )
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

    async fn fetch_symbol_meta(
        &self,
        symbols: &[Symbol],
    ) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.get_all_exchange_info().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all
            .into_iter()
            .filter(|m| symbol_set.contains(&m.symbol))
            .collect())
    }
}

#[async_trait]
impl crate::exchange::FundingFeeSource for BinanceClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    /// 查询资费历史
    ///
    /// 调用 `GET /fapi/v1/income?incomeType=FUNDING_FEE`，返回区间内所有结算明细。
    /// 非当前 quote 币种（如币本位合约）会被自动跳过。
    ///
    /// 同一笔记录的 `tran_id` 在交易所侧唯一，调用方负责去重。
    async fn fetch_funding_fees(
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
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(30)]).await?;
        let query = self
            .build_signed_query(&[
                ("incomeType", "FUNDING_FEE"),
                ("startTime", &start_str),
                ("endTime", &end_str),
                ("limit", "1000"),
            ])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = crate::net::send(
            self.client
                .get(format!("{}/fapi/v1/income?{}", self.base_url, query))
                .header("X-MBX-APIKEY", api_key),
            admitted,
        )
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
}

#[async_trait]
impl AccountClient for BinanceClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn cancel_order(
        &self,
        symbol: &Symbol,
        client_order_id: &str,
    ) -> Result<(), ExchangeError> {
        let api_key = self
            .api_key()
            .ok_or_else(|| ExchangeError::Other("No API key".to_string()))?;

        let admitted = LIMITER.action(&[IP_WEIGHT.cost(1)])?;
        let inst = to_binance(symbol, &self.quote);
        let query = self
            .build_signed_query(&[("symbol", &inst), ("origClientOrderId", client_order_id)])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = crate::net::send(
            self.client
                .delete(format!("{}/fapi/v1/order?{}", self.base_url, query))
                .header("X-MBX-APIKEY", api_key),
            admitted,
        )
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

    async fn fetch_pending_orders(
        &self,
        symbol: &Symbol,
    ) -> Result<Vec<crate::domain::OrderUpdate>, ExchangeError> {
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
            orig_qty: String,
            executed_qty: String,
        }

        // 带 symbol：权重 1
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(1)]).await?;
        let inst = to_binance(symbol, &self.quote);
        let query = self
            .build_signed_query(&[("symbol", &inst)])
            .ok_or_else(|| ExchangeError::Other("Failed to sign request".to_string()))?;

        let resp = crate::net::send(
            self.client
                .get(format!("{}/fapi/v1/openOrders?{}", self.base_url, query))
                .header("X-MBX-APIKEY", api_key),
            admitted,
        )
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
        fn parse_field(raw: &str, field: &str) -> Result<Decimal, ExchangeError> {
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
            // openOrders 只返回未终结的单，故只有这两种状态；其余按未知跳过而非猜测
            let status = match o.status.as_str() {
                "NEW" => OrderStatus::Pending,
                "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
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
                quantity: parse_field(&o.orig_qty, "origQty")?,
                filled_quantity: parse_field(&o.executed_qty, "executedQty")?,
            });
        }

        Ok(updates)
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let api_key = self
            .api_key()
            .ok_or_else(|| PlaceError::unsent("No API key"))?;

        let symbol = to_binance(&order.symbol, &self.quote);
        let side = side_to_binance(order.side);
        let (order_type, price, tif) = order_type_to_binance(&order.order_type);
        let qty = crate::domain::to_wire(order.quantity);
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

        // 下单不占 IP 权重，占账户下单数；签名放在取额度之后
        let admitted = LIMITER.action(&[ORDERS_10S.cost(1), ORDERS_1M.cost(1)])?;
        let query = self
            .build_signed_query(&params)
            .ok_or_else(|| PlaceError::unsent("Failed to sign request"))?;

        let resp = crate::net::send(
            self.client
                .post(format!("{}/fapi/v1/order?{}", self.base_url, query))
                .header("X-MBX-APIKEY", api_key),
            admitted,
        )
        .await
        .map_err(|e| place_send_error(Exchange::Binance, e))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| {
            PlaceError::indeterminate(format!("Binance 读下单回复失败 (HTTP {status}): {e}"))
        })?;
        parse_place_reply(status, &text)
    }

    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        self.get_account_info().await
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        self.fetch_positions_impl().await
    }
}

/// 币安自己说「执行状态未知」的错误码（官方文档）：
/// -1000 UNKNOWN、-1001 DISCONNECTED（内部错误）、-1006 UNEXPECTED_RESP、-1007 TIMEOUT
/// （"Send status unknown; execution status unknown"）。
const BINANCE_UNKNOWN_STATUS_CODES: &[i32] = &[-1000, -1001, -1006, -1007];

/// 下单回复 → 三种结论。纯函数，按币安文档的约定：
/// - 2xx 带 `orderId`：成功；2xx 但解析不了：单已受理却拿不到编号，结果不确定；
/// - 回复是 `{code, msg}`：`code` 属于「状态未知」那几个 → 不确定；-1008（服务过载，文档写明
///   请求失败）→ 拒绝；其余 5xx 与 408（等后端超时）→ 不确定（文档：不代表失败，可能已执行）；其余 4xx → 拒绝
///   （格式/精度/余额/限频/只挂 maker 会吃单 -5022 等，单没进撮合）；
/// - 非 2xx 又解析不出 `{code, msg}`（网关 HTML、空回复）：不确定。
fn parse_place_reply(status: u16, text: &str) -> Result<OrderId, PlaceError> {
    #[derive(Deserialize)]
    struct Accepted {
        #[serde(rename = "orderId")]
        order_id: i64,
    }
    #[derive(Deserialize)]
    struct Failed {
        code: i32,
        msg: String,
    }
    let body: String = text.chars().take(300).collect();
    if (200..300).contains(&status) {
        return serde_json::from_str::<Accepted>(text)
            .map(|a| a.order_id.to_string())
            .map_err(|e| {
                PlaceError::indeterminate(format!(
                    "Binance 下单回复解析不了 (HTTP {status}): {e}; body: {body}"
                ))
            });
    }
    let Ok(Failed { code, msg }) = serde_json::from_str::<Failed>(text) else {
        return Err(PlaceError::indeterminate(format!(
            "Binance 下单失败且回复解析不了 (HTTP {status}): {body}"
        )));
    };
    let detail = format!("HTTP {status}, code={code}, msg={msg}");
    if BINANCE_UNKNOWN_STATUS_CODES.contains(&code) {
        return Err(PlaceError::indeterminate(format!("Binance {detail}")));
    }
    if code == -2022 {
        // "ReduceOnly Order is rejected" —— reduce-only 单因仓位已平被拒
        return Err(PlaceError::Rejected(RejectReason::ReduceOnlyClosed));
    }
    if (status >= 500 || status == 408) && code != -1008 {
        return Err(PlaceError::indeterminate(format!("Binance {detail}")));
    }
    Err(PlaceError::Rejected(RejectReason::Other(detail)))
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
            ("LIMIT", Some(crate::domain::to_wire(*price)), Some(tif_str))
        }
    }
}

/// 解析 `/fapi/v1/fundingRate` 的回复。`symbol` 是请求时的内部符号（回复里的是交易所写法，
/// 不再反解一遍）。费率解析失败整批报错：少一条结算会让 24h 累计静默偏小。
fn parse_settled_funding(
    text: &str,
    symbol: &Symbol,
) -> Result<Vec<SettledFunding>, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        funding_time: u64,
        funding_rate: String,
    }
    let entries: Vec<Entry> = serde_json::from_str(text).map_err(|e| {
        ExchangeError::ParseError(format!(
            "fundingRate: {e} (body: {})",
            text.chars().take(300).collect::<String>()
        ))
    })?;
    entries
        .into_iter()
        .map(|e| {
            let rate = e.funding_rate.parse::<f64>().map_err(|_| {
                ExchangeError::ParseError(format!("fundingRate 非法: {}", e.funding_rate))
            })?;
            Ok(SettledFunding {
                exchange: Exchange::Binance,
                symbol: symbol.clone(),
                rate,
                settle_time: e.funding_time,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl crate::exchange::SettledFundingSource for BinanceClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    /// 拉取某交易对自 `start_ms` 起**已结算**的公共资金费率（`GET /fapi/v1/fundingRate`，
    /// 无需凭证），按结算时间升序。
    ///
    /// 限频由本所闸门管（[`FUNDING_RATE`]）。单次最多 1000 条，24h 窗口最多 24 条（1h 周期）。
    async fn fetch_settled_funding(
        &self,
        symbol: &Symbol,
        start_ms: Timestamp,
    ) -> Result<Vec<SettledFunding>, ExchangeError> {
        let url = format!(
            "{}/fapi/v1/fundingRate?symbol={}&startTime={}&limit=1000",
            self.base_url,
            to_binance(symbol, &self.quote),
            start_ms
        );
        let admitted = LIMITER
            .query(&[FUNDING_RATE.cost(1), IP_WEIGHT.cost(1)])
            .await?;
        let resp = crate::net::send(self.client.get(&url), admitted)
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
        parse_settled_funding(&text, symbol)
    }
}

#[cfg(test)]
mod settled_funding_tests {
    use super::*;

    #[test]
    fn parses_public_funding_history() {
        let body = r#"[{"symbol":"BTCUSDT","fundingTime":1794412800000,"fundingRate":"0.00006639","markPrice":"73953.1","rateType":"Regular"},
                       {"symbol":"BTCUSDT","fundingTime":1794441600000,"fundingRate":"-0.00003453","markPrice":"73979.9"}]"#;
        let got = parse_settled_funding(body, &"BTC".to_string()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].settle_time, 1794412800000);
        assert!((got[1].rate + 0.00003453).abs() < 1e-15);
        assert_eq!(got[1].exchange, Exchange::Binance);
    }

    #[test]
    fn bad_rate_fails_the_whole_batch() {
        let body = r#"[{"fundingTime":1,"fundingRate":"x"}]"#;
        assert!(parse_settled_funding(body, &"BTC".to_string()).is_err());
    }
}

#[cfg(test)]
mod exchange_info_tests {
    use super::*;

    /// 最小下单金额取自 MIN_NOTIONAL 过滤器（字段形状照 11-12 实测 fapi/v1/exchangeInfo）；
    /// 缺这个过滤器的品种剔除，不给出一份「没有金额下限」的假规格
    #[test]
    fn min_notional_comes_from_the_filter_and_is_required() {
        let body = r#"{"symbols":[
            {"symbol":"BTCUSDT","filters":[
                {"filterType":"PRICE_FILTER","tickSize":"0.10","minPrice":"1","maxPrice":"1000000"},
                {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001","maxQty":"1000"},
                {"filterType":"MIN_NOTIONAL","notional":"50"}]},
            {"symbol":"XMRUSDT","filters":[
                {"filterType":"PRICE_FILTER","tickSize":"0.01"},
                {"filterType":"LOT_SIZE","stepSize":"0.001","minQty":"0.001"}]}
        ]}"#;
        let metas = parse_exchange_info(body, "USDT").unwrap();
        assert_eq!(metas.len(), 1, "缺 MIN_NOTIONAL 的 XMR 应被剔除");
        assert_eq!(metas[0].symbol, "BTC");
        assert_eq!(metas[0].min_notional, 50.0);
    }
}

#[cfg(test)]
mod place_reply_tests {
    use super::*;

    fn rejected(r: Result<OrderId, PlaceError>) -> bool {
        matches!(r, Err(PlaceError::Rejected(_)))
    }
    fn indeterminate(r: Result<OrderId, PlaceError>) -> bool {
        matches!(r, Err(PlaceError::Indeterminate(_)))
    }

    #[test]
    fn accepted_returns_order_id() {
        assert_eq!(
            parse_place_reply(200, r#"{"orderId":123,"status":"NEW"}"#),
            Ok("123".to_string())
        );
    }

    /// 受理了却拿不到编号：单可能在簿上，不能当没挂上
    #[test]
    fn unparseable_2xx_is_indeterminate() {
        assert!(indeterminate(parse_place_reply(200, "<html>")));
    }

    /// 交易所明确拒单：精度、余额、只挂 maker 会吃单、限频、过载（文档写明失败）
    #[test]
    fn explicit_rejections() {
        for (status, body) in [
            (
                400,
                r#"{"code":-1111,"msg":"Precision is over the maximum defined for this asset."}"#,
            ),
            (400, r#"{"code":-2019,"msg":"Margin is insufficient."}"#),
            (
                400,
                r#"{"code":-5022,"msg":"Due to the order could not be executed as maker, the Post Only order will be rejected."}"#,
            ),
            (429, r#"{"code":-1003,"msg":"Too many requests"}"#),
            (
                503,
                r#"{"code":-1008,"msg":"Server is currently overloaded with other requests."}"#,
            ),
        ] {
            assert!(rejected(parse_place_reply(status, body)), "{status} {body}");
        }
    }

    #[test]
    fn reduce_only_closed_keeps_its_reason() {
        assert_eq!(
            parse_place_reply(
                400,
                r#"{"code":-2022,"msg":"ReduceOnly Order is rejected."}"#
            ),
            Err(PlaceError::Rejected(RejectReason::ReduceOnlyClosed))
        );
    }

    /// 币安自己说状态未知的码、5xx、408、解析不了的失败回复 → 不确定
    #[test]
    fn unknown_outcomes() {
        for (status, body) in [
            (
                400,
                r#"{"code":-1007,"msg":"Timeout waiting for response from backend server. Send status unknown; execution status unknown."}"#,
            ),
            (
                400,
                r#"{"code":-1006,"msg":"An unexpected response was received from the message bus."}"#,
            ),
            (
                400,
                r#"{"code":-1000,"msg":"An unknown error occurred while processing the request."}"#,
            ),
            (
                400,
                r#"{"code":-1001,"msg":"Internal error; unable to process your request."}"#,
            ),
            (
                503,
                r#"{"code":-1000,"msg":"Unknown error, please check your request or try again later."}"#,
            ),
            (502, r#"{"code":-1,"msg":"x"}"#),
            (408, r#"{"code":-1,"msg":"timeout"}"#),
            (502, "<html>Bad Gateway</html>"),
            (403, ""),
        ] {
            assert!(
                indeterminate(parse_place_reply(status, body)),
                "{status} {body}"
            );
        }
    }
}

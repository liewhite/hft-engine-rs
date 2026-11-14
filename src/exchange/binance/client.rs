//! Binance U 本位合约 REST：公共接口 [`BinanceClient`]（不要凭证）与账户接口 [`BinanceAccount`]
//! （只有配了凭证才有）。
//!
//! 「有没有凭证」由类型表达：账户类请求只在 [`BinanceAccount`] 上，它的凭证不是 `Option`，
//! 没有「每个私有方法现场判一次有没有 key」的分支。签名请求只有 [`BinanceAccount::signed`]
//! 一处，回复的状态检查与解析只有 [`call`] 一处（#813）。

use super::symbol::{from_binance, to_binance};
use crate::domain::PositiveDecimal;
use crate::domain::{
    now_ms, Decimal, Exchange, ExchangeError, FundingFee, Money, OrderId, OrderType, PlaceError,
    PriceStep, RejectReason, SettledFunding, Side, Symbol, SymbolMeta, TimeInForce, Timestamp,
};
pub use crate::exchange::binance::BinanceCredentials;
use crate::exchange::binance::{
    FUNDING_RATE, IP_WEIGHT, LIMITER, ORDERS_10S, ORDERS_1M, REST_BASE_URL,
};
use crate::exchange::client::{
    place_send_error, AccountClient, ExchangeClient, ExchangeOrder, OpenOrder,
};
use crate::exchange::rate_limit::Admitted;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::{Client, Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;

// ============================================================================
// 请求与回复（公共、账户共用）
// ============================================================================

/// 发一个请求，拿回 2xx 的回复正文。没拿到回复 → 请求失败；非 2xx → 交易所的
/// `{code, msg}`（解析不了就带 HTTP 状态与正文开头）。
async fn call(req: RequestBuilder, admitted: Admitted) -> Result<String, ExchangeError> {
    let transport = |e| ExchangeError::transport(Exchange::Binance, e);
    let resp = crate::net::send(req, admitted).await.map_err(transport)?;
    let status = resp.status();
    let text = resp.text().await.map_err(transport)?;
    if !status.is_success() {
        #[derive(Deserialize)]
        struct Failed {
            code: i32,
            msg: String,
        }
        return Err(match serde_json::from_str::<Failed>(&text) {
            Ok(f) => ExchangeError::api(Exchange::Binance, f.code, f.msg),
            Err(_) => ExchangeError::api(Exchange::Binance, status, preview(&text)),
        });
    }
    Ok(text)
}

/// 解析 2xx 回复正文；解析不了是回复格式不对（不是连接问题），带上正文开头
fn parse<T: DeserializeOwned>(what: &str, text: &str) -> Result<T, ExchangeError> {
    serde_json::from_str(text).map_err(|e| {
        ExchangeError::new(format!(
            "Binance {what} 回复解析失败: {e}; body: {}",
            preview(text)
        ))
    })
}

fn preview(text: &str) -> String {
    text.chars().take(300).collect()
}

fn url(path: &str) -> String {
    format!("{REST_BASE_URL}{path}")
}

// ============================================================================
// 公共接口
// ============================================================================

/// Binance 公共 REST（行情、合约规格、已结算资金费率），不需要凭证
pub struct BinanceClient {
    http: Client,
    /// 计价币种 (e.g., "USDT")
    quote: String,
}

impl BinanceClient {
    pub fn new(quote: String) -> Result<Self, ExchangeError> {
        Ok(Self {
            http: crate::exchange::client::exchange_http_client(Exchange::Binance)?,
            quote,
        })
    }

    /// 计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 获取所有交易对信息
    async fn get_all_exchange_info(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(1)]).await?;
        let text = call(self.http.get(url("/fapi/v1/exchangeInfo")), admitted).await?;
        parse_exchange_info(&text, &self.quote)
    }
}

/// 解析 `fapi/v1/exchangeInfo`：只收配置计价币的永续，缺任一必需过滤器的品种剔除并记 error。
fn parse_exchange_info(text: &str, quote: &str) -> Result<Vec<SymbolMeta>, ExchangeError> {
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
        Price {
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

    let info: ExchangeInfo = parse("exchangeInfo", text)?;

    // 剔除必须留痕（与 OKX、HL 同口径）：被剔掉的 symbol 会一路静默失效，
    // 而三种症状（下单被拒 / 订阅被忽略 / 回报无法折算）看不出共同的根因。
    // 这里只记录不报错，真正的拦截在投产期 `validate_subscriptions`。
    let mut metas = Vec::new();
    for s in info.symbols {
        let Some(symbol) = from_binance(&s.symbol, quote) else {
            continue; // 非当前 quote 的交易对，不属于本引擎的宇宙
        };
        let positive = |raw: &str| {
            crate::domain::parse_decimal(raw)
                .ok()
                .and_then(PositiveDecimal::new)
        };
        let mut price_step: Option<PositiveDecimal> = None;
        let mut size_step: Option<PositiveDecimal> = None;
        let mut min_order_size: Option<PositiveDecimal> = None;
        let mut min_notional: Option<f64> = None;
        for filter in s.filters {
            match filter {
                Filter::Price { tick_size } => price_step = positive(&tick_size),
                Filter::LotSize { step_size, min_qty } => {
                    size_step = positive(&step_size);
                    min_order_size = positive(&min_qty);
                }
                Filter::MinNotional { notional } => {
                    min_notional = notional.parse().ok().filter(|v: &f64| *v > 0.0);
                }
                Filter::Other => {}
            }
        }

        match (price_step, size_step, min_order_size, min_notional) {
            (Some(price_step), Some(size_step), Some(min_order_size), Some(min_notional)) => {
                metas.push(SymbolMeta {
                    exchange: Exchange::Binance,
                    symbol,
                    price_rule: Arc::new(PriceStep::new(price_step)),
                    size_step,
                    min_order_size,
                    min_notional,
                    contract_size: PositiveDecimal::ONE,
                    quote: quote.to_string(),
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

#[async_trait]
impl ExchangeClient for BinanceClient {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        self.get_all_exchange_info().await
    }
}

/// 从 `fapi/v3/account` 回复里取计价币那一项资产的 `marginBalance`，见 [`BinanceAccount::fetch_equity`]
fn quote_margin_balance(text: &str, quote: &str) -> Result<Money, ExchangeError> {
    #[derive(Deserialize)]
    struct AccountResponse {
        assets: Vec<AssetBalance>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AssetBalance {
        asset: String,
        margin_balance: String,
    }
    let account: AccountResponse = parse("account", text)?;
    let a = account
        .assets
        .iter()
        .find(|a| a.asset == quote)
        .ok_or_else(|| {
            ExchangeError::new(format!("Binance account assets 里没有计价币 {quote}"))
        })?;
    let amount = a.margin_balance.parse::<f64>().map_err(|_| {
        ExchangeError::new(format!(
            "Binance account {quote} marginBalance 非法: {}",
            a.margin_balance
        ))
    })?;
    Ok(Money::new(amount, quote))
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
    let entries: Vec<Entry> = parse("fundingRate", text)?;
    entries
        .into_iter()
        .map(|e| {
            let rate = e.funding_rate.parse::<f64>().map_err(|_| {
                ExchangeError::new(format!("Binance fundingRate 非法: {}", e.funding_rate))
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
        let req = self.http.get(url("/fapi/v1/fundingRate")).query(&[
            ("symbol", to_binance(symbol, &self.quote)),
            ("startTime", start_ms.to_string()),
            ("limit", "1000".to_string()),
        ]);
        let admitted = LIMITER
            .query(&[FUNDING_RATE.cost(1), IP_WEIGHT.cost(1)])
            .await?;
        parse_settled_funding(&call(req, admitted).await?, symbol)
    }
}

// ============================================================================
// 账户接口
// ============================================================================

/// Binance 账户 REST（下单、撤单、持仓、净值、资金费流水、ListenKey）。只有配了凭证才构造，
/// 持有它即「该所有账户」（见 [`AccountClient`]）。
pub struct BinanceAccount {
    http: Client,
    quote: String,
    credentials: BinanceCredentials,
}

impl BinanceAccount {
    /// 与公共接口共用同一个 HTTP client 与计价币
    pub fn new(public: &BinanceClient, credentials: BinanceCredentials) -> Self {
        Self {
            http: public.http.clone(),
            quote: public.quote.clone(),
            credentials,
        }
    }

    /// 签名请求（`HMAC-SHA256(query)`，`timestamp` 与 `signature` 附在 query 末尾）。
    /// 本所全部签名请求都经这里。
    fn signed(&self, method: Method, path: &str, params: &[(&str, &str)]) -> RequestBuilder {
        let mut query: String = params.iter().map(|(k, v)| format!("{k}={v}&")).collect();
        query.push_str(&format!("timestamp={}", now_ms()));
        let signature = sign(&self.credentials.secret, &query);
        self.with_key(self.http.request(
            method,
            format!("{}?{query}&signature={signature}", url(path)),
        ))
    }

    /// 只要 API key、不要签名的请求（ListenKey）
    fn with_key(&self, req: RequestBuilder) -> RequestBuilder {
        req.header("X-MBX-APIKEY", &self.credentials.api_key)
    }

    /// 创建（或取回已有的）私有流 ListenKey
    pub(crate) async fn create_listen_key(&self) -> Result<String, ExchangeError> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "listenKey")]
            listen_key: String,
        }
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(1)]).await?;
        let text = call(
            self.with_key(self.http.post(url("/fapi/v1/listenKey"))),
            admitted,
        )
        .await?;
        Ok(parse::<Response>("listenKey", &text)?.listen_key)
    }

    /// 续期 ListenKey（有效期从此再延 60 分钟）
    pub(crate) async fn keepalive_listen_key(&self) -> Result<(), ExchangeError> {
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(1)]).await?;
        call(
            self.with_key(self.http.put(url("/fapi/v1/listenKey"))),
            admitted,
        )
        .await?;
        Ok(())
    }

    /// 签名 GET：先排队取 `weight` 的 IP 权重，返回 2xx 正文
    async fn signed_get(
        &self,
        path: &str,
        params: &[(&str, &str)],
        weight: u32,
    ) -> Result<String, ExchangeError> {
        let admitted = LIMITER.query(&[IP_WEIGHT.cost(weight)]).await?;
        call(self.signed(Method::GET, path, params), admitted).await
    }

    /// 撤单（交易动作：没额度就失败，不排队）
    async fn cancel(&self, params: &[(&str, &str)]) -> Result<(), ExchangeError> {
        let admitted = LIMITER.action(&[IP_WEIGHT.cost(1)])?;
        call(
            self.signed(Method::DELETE, "/fapi/v1/order", params),
            admitted,
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl crate::exchange::FundingFeeSource for BinanceAccount {
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
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct IncomeEntry {
            symbol: String,
            income: String,
            asset: String,
            time: u64,
            tran_id: u64,
        }

        let (start, end) = (start_ms.to_string(), end_ms.to_string());
        let text = self
            .signed_get(
                "/fapi/v1/income",
                &[
                    ("incomeType", "FUNDING_FEE"),
                    ("startTime", &start),
                    ("endTime", &end),
                    ("limit", "1000"),
                ],
                30,
            )
            .await?;
        let entries: Vec<IncomeEntry> = parse("income", &text)?;

        let mut result = Vec::with_capacity(entries.len());
        for e in entries {
            let Some(symbol) = from_binance(&e.symbol, &self.quote) else {
                continue; // 跨 quote 合约（如币本位），跳过
            };
            let amount: f64 = e.income.parse().map_err(|_| {
                ExchangeError::new(format!(
                    "Binance 资金费金额非法 '{}' ({})",
                    e.income, e.symbol
                ))
            })?;
            result.push(FundingFee {
                exchange: Exchange::Binance,
                symbol,
                amount: Money::new(amount, e.asset),
                timestamp: e.time,
                tran_id: e.tran_id,
            });
        }
        Ok(result)
    }
}

#[async_trait]
impl AccountClient for BinanceAccount {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    async fn cancel_order(
        &self,
        symbol: &Symbol,
        client_order_id: &str,
    ) -> Result<(), ExchangeError> {
        let inst = to_binance(symbol, &self.quote);
        self.cancel(&[("symbol", &inst), ("origClientOrderId", client_order_id)])
            .await
    }

    async fn fetch_open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Row {
            symbol: String,
            order_id: i64,
        }
        // 不带 symbol：返回 U 本位合约账户的全部挂单，权重 40
        let text = self.signed_get("/fapi/v1/openOrders", &[], 40).await?;
        let rows: Vec<Row> = parse("openOrders", &text)?;
        Ok(rows
            .into_iter()
            .map(|r| OpenOrder {
                instrument: r.symbol,
                order_id: r.order_id.to_string(),
            })
            .collect())
    }

    async fn cancel_open_order(&self, order: &OpenOrder) -> Result<(), ExchangeError> {
        self.cancel(&[
            ("symbol", order.instrument.as_str()),
            ("orderId", order.order_id.as_str()),
        ])
        .await
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let symbol = to_binance(&order.symbol, &self.quote);
        let qty = crate::domain::to_wire(order.quantity);
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", symbol),
            ("side", side_to_binance(order.side).to_string()),
            ("quantity", qty),
            ("reduceOnly", order.reduce_only.to_string()),
            ("newClientOrderId", order.client_order_id.clone()),
        ];
        match &order.order_type {
            OrderType::Market => params.push(("type", "MARKET".to_string())),
            OrderType::Limit { price, tif } => {
                params.push(("type", "LIMIT".to_string()));
                params.push(("price", crate::domain::to_wire(*price)));
                params.push(("timeInForce", tif_to_binance(*tif).to_string()));
            }
        }
        let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();

        // 下单不占 IP 权重，占账户下单数；签名放在取额度之后
        let admitted = LIMITER.action(&[ORDERS_10S.cost(1), ORDERS_1M.cost(1)])?;
        let resp = crate::net::send(
            self.signed(Method::POST, "/fapi/v1/order", &params),
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

    /// 净值 = `fapi/v3/account` 里计价币那一项资产的 `marginBalance`（钱包余额 + 未实现盈亏）。
    /// 不用顶层 `totalMarginBalance`：联合保证金模式下它是各币折成美元的合计。`assets` 是全量列表
    /// （实测 11-14 含余额为 0 的币），找不到计价币是接口变了，报错。
    async fn fetch_equity(&self) -> Result<Money, ExchangeError> {
        let text = self.signed_get("/fapi/v3/account", &[], 5).await?;
        quote_margin_balance(&text, &self.quote)
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PositionInfo {
            symbol: String,
            position_amt: String,
            /// 单向模式恒为 "BOTH"；双向（对冲）模式为 "LONG"/"SHORT"
            position_side: String,
        }

        let text = self.signed_get("/fapi/v2/positionRisk", &[], 5).await?;
        let positions: Vec<PositionInfo> = parse("positionRisk", &text)?;

        let mut result = Vec::new();
        for p in positions {
            // 双向（对冲）持仓模式下同一 symbol 返回 LONG/SHORT 两行，照单全收会产出
            // 两条同 symbol 的 Position —— 基线与对账全部串账。本系统按单向净持仓建模，
            // 检测到双向模式直接报错拒绝（与 OKX 适配层在 codec 里拒绝 long/short
            // posSide 的立场一致），由使用者去交易所切换持仓模式。
            if p.position_side != "BOTH" {
                return Err(ExchangeError::new(format!(
                    "Binance 账户处于双向持仓模式（{} positionSide={}），本系统只支持单向\
                     净持仓，请在交易所切换为单向模式",
                    p.symbol, p.position_side
                )));
            }
            let Some(symbol) = from_binance(&p.symbol, &self.quote) else {
                continue; // 非当前 quote 的交易对，跳过
            };
            let size: Decimal = crate::domain::parse_decimal(&p.position_amt).map_err(|_| {
                ExchangeError::new(format!(
                    "Binance positionAmt 非法 '{}' ({})",
                    p.position_amt, p.symbol
                ))
            })?;
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
}

/// 签名 = hex(HMAC-SHA256(secret, query))
fn sign(secret: &str, query: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC 接受任意长度的密钥");
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
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
    let body = preview(text);
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
    Err(PlaceError::rejected(detail))
}

fn side_to_binance(side: Side) -> &'static str {
    match side {
        Side::Long => "BUY",
        Side::Short => "SELL",
    }
}

fn tif_to_binance(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::GTC => "GTC",
        TimeInForce::IOC => "IOC",
        TimeInForce::FOK => "FOK",
        TimeInForce::PostOnly => "GTX",
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
mod sign_tests {
    /// 官方文档（SIGNED Endpoint Examples）的样例：密钥、query 与签名照抄
    #[test]
    fn signature_matches_the_documented_example() {
        assert_eq!(
            super::sign(
                "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j",
                "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559"
            ),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
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

#[cfg(test)]
mod equity_tests {
    use super::*;

    /// 净值取计价币那一项的 marginBalance、币种照填；不读折算成美元的顶层合计
    #[test]
    fn equity_is_the_quote_asset_margin_balance() {
        let text = r#"{"totalMarginBalance":"999","assets":[
            {"asset":"BNB","marginBalance":"0.5"},
            {"asset":"USDT","marginBalance":"972.82873309"},
            {"asset":"USDC","marginBalance":"0"}]}"#;
        assert_eq!(
            quote_margin_balance(text, "USDT").unwrap(),
            Money::new(972.82873309, "USDT")
        );
        assert!(quote_margin_balance(text, "FDUSD").is_err());
    }
}

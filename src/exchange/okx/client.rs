//! OKX REST：公共接口 [`OkxClient`]（合约目录、已结算资金费率，不要凭证）与账户接口
//! [`OkxAccount`]（只有配了凭证才有）。
//!
//! 「有没有凭证」由类型表达：账户类请求只在 [`OkxAccount`] 上，它的凭证不是 `Option`。
//! 签名请求只有 [`OkxAccount::signed`] 一处，回复信封 `{code, msg, data}` 的解析只有
//! [`envelope`] 一处（#813）。

use super::symbol::{from_okx, to_okx};
use crate::domain::PositiveDecimal;
use crate::domain::{
    Decimal, Exchange, ExchangeError, FundingFee, Greeks, Money, OrderId, OrderType, PlaceError,
    PriceStep, RejectReason, Side, Symbol, SymbolMeta, TimeInForce, Timestamp,
};
use crate::exchange::client::{
    place_send_error, AccountClient, ExchangeClient, ExchangeOrder, OpenOrder,
};
use crate::exchange::okx::codec::{parse_quote_equity, BalanceDetail, GreeksData, PositionData};
pub use crate::exchange::okx::OkxCredentials;
use crate::exchange::okx::{
    BALANCE, BILLS, CANCEL, FUNDING_HISTORY, GREEKS, INSTRUMENTS, LIMITER, ORDERS_PENDING, PLACE,
    PLACE_ACCOUNT, POSITIONS, REST_BASE_URL,
};
use crate::exchange::rate_limit::{Admitted, Quota};
use async_trait::async_trait;
use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ============================================================================
// 请求与回复（公共、账户共用）
// ============================================================================

/// OKX 的回复信封。失败也常带非 2xx 状态，所以先看正文、不看状态码
#[derive(Deserialize)]
struct Envelope<T> {
    code: String,
    #[serde(default)]
    msg: String,
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

impl<T> Envelope<T> {
    /// 顶层 `code` 非 0 即错误
    fn into_data(self) -> Result<Vec<T>, ExchangeError> {
        if self.code != "0" {
            return Err(ExchangeError::api(Exchange::OKX, self.code, self.msg));
        }
        Ok(self.data)
    }
}

/// 发一个请求，把正文解析成信封（不看顶层 `code`，由调用方决定怎么看）。
/// 没拿到回复 → 请求失败；正文不是信封（网关 HTML、空回复）→ 带 HTTP 状态与正文开头报错。
async fn envelope<T: DeserializeOwned>(
    what: &str,
    req: RequestBuilder,
    admitted: Admitted,
) -> Result<Envelope<T>, ExchangeError> {
    let transport = |e| ExchangeError::transport(Exchange::OKX, e);
    let resp = crate::net::send(req, admitted).await.map_err(transport)?;
    let status = resp.status();
    let text = resp.text().await.map_err(transport)?;
    serde_json::from_str(&text).map_err(|e| {
        ExchangeError::new(format!(
            "OKX {what} 回复解析失败 (HTTP {status}): {e}; body: {}",
            preview(&text)
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

/// OKX 公共 REST（合约目录、已结算资金费率），不需要凭证
pub struct OkxClient {
    /// 合约目录（含每张币数 ctVal），进程内只拉一次：引擎预加载的品种元数据与账户接口
    /// 折算张数用的每张币数都取自这一份，不存在两份各拉各的。
    ///
    /// [`AccountClient`] 的契约是**返回币本位数量** (见 [`crate::domain::Quantity`])，而 OKX
    /// REST 返回的是张数，故适配层必须自备折算能力，不能把折算责任推给调用方 —— 那样每个
    /// 调用点都得记着乘一次，漏一处就静默算错 contract_size 倍。
    instruments: tokio::sync::OnceCell<Vec<SymbolMeta>>,
    http: Client,
    /// 计价币种 (e.g., "USDT")
    quote: String,
}

impl OkxClient {
    pub fn new(quote: String) -> Result<Self, ExchangeError> {
        Ok(Self {
            instruments: tokio::sync::OnceCell::new(),
            http: crate::exchange::client::exchange_http_client(Exchange::OKX)?,
            quote,
        })
    }

    /// 计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 合约目录：首次调用时拉取，之后取缓存
    async fn instruments(&self) -> Result<&[SymbolMeta], ExchangeError> {
        self.instruments
            .get_or_try_init(|| self.get_all_instruments())
            .await
            .map(Vec::as_slice)
    }

    /// 取该 symbol 的合约乘数 (张 -> 币)，读合约目录
    async fn contract_size_of(&self, symbol: &Symbol) -> Result<Decimal, ExchangeError> {
        self.instruments()
            .await?
            .iter()
            .find(|m| &m.symbol == symbol)
            .map(|m| m.contract_size.get())
            .ok_or_else(|| ExchangeError::new(format!("OKX 合约目录里没有 {symbol}")))
    }

    /// 获取所有交易对信息 (公开接口)
    async fn get_all_instruments(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InstrumentData {
            inst_id: String,
            tick_sz: String,
            lot_sz: String,
            min_sz: String,
            ct_val: String,
        }

        let admitted = LIMITER.query(&[INSTRUMENTS.cost(1)]).await?;
        let req = self
            .http
            .get(url("/api/v5/public/instruments"))
            .query(&[("instType", "SWAP")]);
        let data: Vec<InstrumentData> =
            envelope("instruments", req, admitted).await?.into_data()?;

        // 剔除必须**留痕**：被剔掉的 symbol 会一路静默失效 —— 下单被 "No SymbolMeta" 拒、
        // 私有回报因无法折算张数被丢弃（持仓从此落后于交易所）、行情订阅被忽略。
        //
        // 这里只记录不报错（与 BN、HL 同口径）：OKX 有数百个合约，个别新品种字段异常不该挡住
        // 整机启动。真正的拦截在投产期 `validate_subscriptions` —— 那里知道哪些 symbol 是
        // **策略要用的**，缺了就拒绝投产（分工：加载处负责可见，投产处负责判定）。
        let mut metas = Vec::new();
        for d in data {
            // 只接入配置计价币的永续（如 BTC-USDT-SWAP），映射规则只在 okx::symbol 一处
            let Some(symbol) = from_okx(&d.inst_id, &self.quote) else {
                continue;
            };
            let positive = |raw: &str| {
                crate::domain::parse_decimal(raw)
                    .ok()
                    .and_then(PositiveDecimal::new)
            };
            match (
                positive(&d.tick_sz),
                positive(&d.lot_sz),
                positive(&d.min_sz),
                positive(&d.ct_val),
            ) {
                (Some(tick), Some(lot), Some(min), Some(ct_val)) => metas.push(SymbolMeta {
                    exchange: Exchange::OKX,
                    symbol,
                    price_rule: Arc::new(PriceStep::new(tick)),
                    size_step: lot,
                    min_order_size: min,
                    min_notional: 0.0,
                    contract_size: ct_val,
                    quote: self.quote.clone(),
                }),
                _ => tracing::error!(
                    inst_id = %d.inst_id,
                    tick_sz = %d.tick_sz, lot_sz = %d.lot_sz,
                    min_sz = %d.min_sz, ct_val = %d.ct_val,
                    "OKX 合约元数据字段异常，该 symbol 不可交易"
                ),
            }
        }

        Ok(metas)
    }
}

#[async_trait]
impl ExchangeClient for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        Ok(self.instruments().await?.to_vec())
    }
}

/// `funding-rate-history` 的单条记录
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingHistoryEntry {
    funding_time: String,
    realized_rate: String,
}

/// 解析 `funding-rate-history`：只留 `fundingTime >= start_ms` 的，按时间升序。
///
/// 返回条数顶到单页上限 100 且最早一条仍晚于 `start_ms` 时报错（窗口没取全）——
/// 不会发生在 ≥1h 结算周期上，真出现说明交易所行为变了，静默少算比报错危险。
fn parse_settled_funding(
    text: &str,
    symbol: &Symbol,
    start_ms: Timestamp,
) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError> {
    let resp: Envelope<FundingHistoryEntry> = serde_json::from_str(text).map_err(|e| {
        ExchangeError::new(format!(
            "OKX funding-rate-history 回复解析失败: {e}; body: {}",
            preview(text)
        ))
    })?;
    let mut out = Vec::new();
    for e in resp.into_data()? {
        let settle_time: u64 = e
            .funding_time
            .parse()
            .map_err(|_| ExchangeError::new(format!("OKX fundingTime 非法: {}", e.funding_time)))?;
        let rate: f64 = e.realized_rate.parse().map_err(|_| {
            ExchangeError::new(format!("OKX realizedRate 非法: {}", e.realized_rate))
        })?;
        out.push(crate::domain::SettledFunding {
            exchange: Exchange::OKX,
            symbol: symbol.clone(),
            rate,
            settle_time,
        });
    }
    let earliest = out.iter().map(|f| f.settle_time).min();
    if out.len() >= 100 && earliest.is_some_and(|t| t > start_ms) {
        return Err(ExchangeError::new(format!(
            "OKX {symbol} 资金费历史一页 100 条未覆盖窗口起点"
        )));
    }
    out.retain(|f| f.settle_time >= start_ms);
    out.sort_by_key(|f| f.settle_time);
    Ok(out)
}

#[async_trait::async_trait]
impl crate::exchange::SettledFundingSource for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    /// 拉取某币自 `start_ms` 起**已结算**的公共资金费率（`GET /api/v5/public/funding-rate-history`，
    /// 无需凭证），按结算时间升序。取 `realizedRate`（实际收付的费率）。
    ///
    /// 单次最多 100 条（最新的在前）；OKX 最短 1h 一结，24h 窗口最多 24 条，一页足够。
    async fn fetch_settled_funding(
        &self,
        symbol: &Symbol,
        start_ms: Timestamp,
    ) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError> {
        let inst_id = to_okx(symbol, &self.quote);
        let admitted = LIMITER
            .query(&[FUNDING_HISTORY.cost_in(&inst_id, 1)])
            .await?;
        let req = self
            .http
            .get(url("/api/v5/public/funding-rate-history"))
            .query(&[("instId", inst_id.as_str()), ("limit", "100")]);
        let transport = |e| ExchangeError::transport(Exchange::OKX, e);
        let resp = crate::net::send(req, admitted).await.map_err(transport)?;
        let text = resp.text().await.map_err(transport)?;
        parse_settled_funding(&text, symbol, start_ms)
    }
}

// ============================================================================
// 账户接口
// ============================================================================

/// OKX 账户 REST（下单、撤单、持仓、净值、资金费流水、希腊值）。只有配了凭证才构造，持有它
/// 即「该所有账户」（见 [`AccountClient`]）。数量折算用公共接口的合约目录（同一份）。
pub struct OkxAccount {
    public: Arc<OkxClient>,
    credentials: OkxCredentials,
    /// 每个请求都带的鉴权头（API key、passphrase），构造时校验一次能否做成 HTTP 头
    auth_headers: HeaderMap,
}

impl OkxAccount {
    pub fn new(public: Arc<OkxClient>, credentials: OkxCredentials) -> Result<Self, ExchangeError> {
        let header = |name: &str, v: &str| {
            HeaderValue::from_str(v).map_err(|_| {
                ExchangeError::new(format!("OKX 凭证的 {name} 含有不能放进 HTTP 头的字符"))
            })
        };
        let mut auth_headers = HeaderMap::new();
        auth_headers.insert("OK-ACCESS-KEY", header("api_key", &credentials.api_key)?);
        auth_headers.insert(
            "OK-ACCESS-PASSPHRASE",
            header("passphrase", &credentials.passphrase)?,
        );
        auth_headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        Ok(Self {
            public,
            credentials,
            auth_headers,
        })
    }

    /// 凭证（私有 WS 登录用）
    pub(crate) fn credentials(&self) -> &OkxCredentials {
        &self.credentials
    }

    /// 签名请求：`OK-ACCESS-SIGN = base64(HMAC-SHA256(secret, 时间戳 + 方法 + 路径(含 query) + 正文))`。
    /// 本所全部签名 REST 请求都经这里。
    fn signed(&self, method: Method, path: &str, body: String) -> RequestBuilder {
        let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let sign = self
            .credentials
            .sign(&format!("{timestamp}{}{path}{body}", method.as_str()));
        let mut headers = self.auth_headers.clone();
        headers.insert(
            "OK-ACCESS-SIGN",
            HeaderValue::from_str(&sign).expect("base64 只含合法的头字符"),
        );
        headers.insert(
            "OK-ACCESS-TIMESTAMP",
            HeaderValue::from_str(&timestamp).expect("ISO 时间戳只含合法的头字符"),
        );
        let req = self.public.http.request(method, url(path)).headers(headers);
        if body.is_empty() {
            req
        } else {
            req.body(body)
        }
    }

    /// 签名 GET（先按 `quota` 排队取额度），返回 `data`；顶层 code 非 0 即错误
    async fn signed_get<T: DeserializeOwned>(
        &self,
        quota: &'static Quota,
        path: &str,
    ) -> Result<Vec<T>, ExchangeError> {
        let admitted = LIMITER.query(&[quota.cost(1)]).await?;
        envelope(
            path,
            self.signed(Method::GET, path, String::new()),
            admitted,
        )
        .await?
        .into_data()
    }

    /// 查询账户希腊值 (GET /api/v5/account/greeks)
    pub async fn fetch_greeks(&self) -> Result<Vec<Greeks>, ExchangeError> {
        let data: Vec<GreeksData> = self.signed_get(&GREEKS, "/api/v5/account/greeks").await?;
        data.iter()
            .map(|item| item.to_greeks().map_err(ExchangeError::new))
            .collect()
    }

    /// 取一页挂单（全部品种；`after` 为游标 = 上一页最后一条的 ordId，`None` 即首页）
    async fn fetch_pending_orders_page(
        &self,
        after: Option<&str>,
    ) -> Result<Vec<PendingOrderData>, ExchangeError> {
        let mut path = format!("/api/v5/trade/orders-pending?limit={PENDING_ORDERS_PAGE_LIMIT}");
        if let Some(cursor) = after {
            path.push_str(&format!("&after={cursor}"));
        }
        self.signed_get(&ORDERS_PENDING, &path).await
    }

    /// 撤一张单（按客户端订单号或交易所订单号）。`Ok` 只表示交易所受理了撤单
    async fn cancel(&self, inst_id: String, order: OrderRef) -> Result<(), ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CancelData {
            s_code: String,
            s_msg: String,
        }

        let admitted = LIMITER.action(&[CANCEL.cost_in(&inst_id, 1)])?;
        let body = serde_json::to_string(&CancelRequest { inst_id, order })
            .expect("撤单请求体只含字符串字段，序列化不会失败");
        let path = "/api/v5/trade/cancel-order";
        let resp: Envelope<CancelData> =
            envelope(path, self.signed(Method::POST, path, body), admitted).await?;
        // 单笔结果比顶层 code 具体（顶层只说「有失败」，原因在 sCode/sMsg）
        match resp.data.first() {
            Some(d) if d.s_code != "0" => {
                Err(ExchangeError::api(Exchange::OKX, &d.s_code, &d.s_msg))
            }
            Some(_) => Ok(()),
            None => resp.into_data().map(|_| ()),
        }
    }
}

/// OKX 撤单请求体
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelRequest {
    inst_id: String,
    #[serde(flatten)]
    order: OrderRef,
}

/// 撤单时指认订单用哪个号（OKX 撤单请求里二选一的字段）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum OrderRef {
    ClOrdId(String),
    OrdId(String),
}

/// OKX `orders-pending` 的单页条数上限（交易所固定值）
const PENDING_ORDERS_PAGE_LIMIT: usize = 100;
/// 分页拉取的页数上限。用尽即报错而非静默截断 —— 1 万张挂单远超任何正常策略规模，
/// 走到这里说明游标没有推进（交易所行为变化），继续循环只会挂死。
const PENDING_ORDERS_MAX_PAGES: usize = 100;

/// OKX 挂单快照的单条记录（REST `orders-pending`）：清场只要合约名与订单号
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingOrderData {
    inst_id: String,
    ord_id: String,
}

#[async_trait]
impl AccountClient for OkxAccount {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn cancel_order(
        &self,
        symbol: &Symbol,
        client_order_id: &str,
    ) -> Result<(), ExchangeError> {
        self.cancel(
            to_okx(symbol, &self.public.quote),
            OrderRef::ClOrdId(client_order_id.to_string()),
        )
        .await
    }

    /// 账户全部品种的挂单，翻页取全：取不全（页数上限用尽）就报错，不返回少了几张的列表
    async fn fetch_open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        let mut orders = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..PENDING_ORDERS_MAX_PAGES {
            let page = self.fetch_pending_orders_page(after.as_deref()).await?;
            // 取满一页说明后面还有：OKX 按 ordId 倒序返回，游标传本页最后一条
            let page_full = page.len() >= PENDING_ORDERS_PAGE_LIMIT;
            let last_ord_id = page.last().map(|d| d.ord_id.clone());
            orders.extend(page.into_iter().map(|d| OpenOrder {
                instrument: d.inst_id,
                order_id: d.ord_id,
            }));
            match last_ord_id {
                Some(cursor) if page_full => after = Some(cursor),
                // 未取满 或 空页：已到末页
                _ => return Ok(orders),
            }
        }
        Err(ExchangeError::new(format!(
            "OKX 挂单分页超过 {PENDING_ORDERS_MAX_PAGES} 页仍未取完，快照不可信"
        )))
    }

    async fn cancel_open_order(&self, order: &OpenOrder) -> Result<(), ExchangeError> {
        self.cancel(
            order.instrument.clone(),
            OrderRef::OrdId(order.order_id.clone()),
        )
        .await
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let inst_id = to_okx(&order.symbol, &self.public.quote);
        let (ord_type, px) = order_type_to_okx(&order.order_type);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct OrderRequest<'a> {
            inst_id: &'a str,
            td_mode: &'static str,
            side: &'static str,
            ord_type: &'static str,
            sz: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            px: Option<String>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            reduce_only: bool,
            cl_ord_id: &'a str,
        }

        let admitted = LIMITER.action(&[PLACE.cost_in(&inst_id, 1), PLACE_ACCOUNT.cost(1)])?;
        let body = serde_json::to_string(&OrderRequest {
            inst_id: &inst_id,
            td_mode: "cross",
            side: side_to_okx(order.side),
            ord_type,
            sz: crate::domain::to_wire(order.quantity),
            px,
            reduce_only: order.reduce_only,
            cl_ord_id: &order.client_order_id,
        })
        .map_err(PlaceError::unsent)?;

        let resp = crate::net::send(
            self.signed(Method::POST, "/api/v5/trade/order", body),
            admitted,
        )
        .await
        .map_err(|e| place_send_error(Exchange::OKX, e))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| {
            PlaceError::indeterminate(format!("OKX 读下单回复失败 (HTTP {status}): {e}"))
        })?;
        parse_place_reply(status, &text)
    }

    /// 净值 = `account/balance?ccy=<计价币>` 里计价币那一项的 `eq`，口径见 [`parse_quote_equity`]
    async fn fetch_equity(&self) -> Result<Money, ExchangeError> {
        #[derive(Deserialize)]
        struct BalanceData {
            details: Vec<BalanceDetail>,
        }
        let quote = &self.public.quote;
        let balance: Vec<BalanceData> = self
            .signed_get(&BALANCE, &format!("/api/v5/account/balance?ccy={quote}"))
            .await?;
        let data = balance
            .first()
            .ok_or_else(|| ExchangeError::new("OKX account/balance 返回空 data"))?;
        Ok(parse_quote_equity(&data.details, quote)?)
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        let data: Vec<PositionData> = self
            .signed_get(&POSITIONS, "/api/v5/account/positions?instType=SWAP")
            .await?;
        let mut positions = Vec::new();
        for (symbol, d) in mapped_positions(&data, &self.public.quote) {
            // 张 -> 币：[`AccountClient`] 的契约是返回币本位（见 crate::domain::Quantity）。
            // 字段口径（空串=0、张数、双向持仓拒绝）全在 codec 里，不在此处重抄一份。
            let contract_size = self.public.contract_size_of(&symbol).await?;
            positions.push(d.to_position(symbol, contract_size)?);
        }
        Ok(positions)
    }
}

/// 持仓列表里本引擎接入的合约及其内部 symbol。别的计价币、币本位、交割合约是别的品种，
/// 不进任何 symbol 的持仓。
fn mapped_positions<'a>(
    data: &'a [PositionData],
    quote: &'a str,
) -> impl Iterator<Item = (Symbol, &'a PositionData)> + 'a {
    data.iter()
        .filter_map(move |d| match from_okx(&d.inst_id, quote) {
            Some(symbol) => Some((symbol, d)),
            None => {
                tracing::debug!(inst_id = %d.inst_id, "OKX 持仓不是本引擎接入的合约，跳过");
                None
            }
        })
}

/// OKX 自己说「结果未知」或系统侧故障、不能断定没受理的错误码（官方错误码表）：
/// 50001 服务暂不可用、50004 接口请求超时（"不代表请求成功或者失败，请检查请求结果"）、
/// 50013 系统繁忙、50026 系统错误、51149 下单超时。可能出现在顶层 `code`，也可能出现在单笔 `sCode`。
const OKX_UNKNOWN_STATUS_CODES: &[&str] = &["50001", "50004", "50013", "50026", "51149"];

/// 下单回复 → 三种结论。纯函数。
///
/// OKX 的失败回复常带非 2xx 状态（如 50004 是 HTTP 400），所以**先看正文、不看状态码**：
/// - 有单笔结果 `data[0]`：`sCode == "0"` 且有订单号 → 成功（没订单号 → 不确定）；`sCode` 属于「结果未知」→ 不确定；
///   51169（要减的方向没有仓位）→ reduce-only 已平；其余 → 拒绝；
/// - 没有单笔结果：顶层 `code` 属于「结果未知」或 HTTP 5xx → 不确定；其余非 0（签名、限频、参数）→ 拒绝；
///   顶层 0 却没有单笔结果 → 不确定；
/// - 正文解析不了（网关 HTML、空回复）→ 不确定。
fn parse_place_reply(status: u16, text: &str) -> Result<OrderId, PlaceError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OrderData {
        #[serde(default)]
        ord_id: String,
        s_code: String,
        #[serde(default)]
        s_msg: String,
    }
    let unknown = |code: &str| OKX_UNKNOWN_STATUS_CODES.contains(&code);
    let Ok(resp) = serde_json::from_str::<Envelope<OrderData>>(text) else {
        return Err(PlaceError::indeterminate(format!(
            "OKX 下单回复解析不了 (HTTP {status}): {}",
            preview(text)
        )));
    };
    if let Some(d) = resp.data.first() {
        let detail = format!("HTTP {status}, sCode={}, sMsg={}", d.s_code, d.s_msg);
        return match d.s_code.as_str() {
            "0" if !d.ord_id.is_empty() => Ok(d.ord_id.clone()),
            "0" => Err(PlaceError::indeterminate(format!(
                "OKX 受理了却没给订单号: {detail}"
            ))),
            c if unknown(c) => Err(PlaceError::indeterminate(format!("OKX {detail}"))),
            // 51169：要减的方向没有仓位（reduce-only 单，仓位已平）
            "51169" => Err(PlaceError::Rejected(RejectReason::ReduceOnlyClosed)),
            _ => Err(PlaceError::rejected(detail)),
        };
    }
    let detail = format!("HTTP {status}, code={}, msg={}", resp.code, resp.msg);
    match resp.code.as_str() {
        "0" => Err(PlaceError::indeterminate(format!(
            "OKX 下单回复没有单笔结果: {detail}"
        ))),
        c if unknown(c) || status >= 500 => Err(PlaceError::indeterminate(format!("OKX {detail}"))),
        _ => Err(PlaceError::rejected(detail)),
    }
}

fn side_to_okx(side: Side) -> &'static str {
    match side {
        Side::Long => "buy",
        Side::Short => "sell",
    }
}

/// 订单类型 → (`ordType`, `px`)
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
            (ord_type, Some(crate::domain::to_wire(*price)))
        }
    }
}

#[async_trait::async_trait]
impl crate::exchange::FundingFeeSource for OkxAccount {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    /// 查询 `[start, end]` 内的资金费结算明细（GET /api/v5/account/bills，type=8，近 7 天内）。
    ///
    /// 金额取 `balChg`（账户余额的带符号变化：正 = 收到，负 = 付出），结算 ID 取 `billId`。
    /// 只返回本适配器计价币的永续（与品种目录同一映射），其余合约的资费不归本引擎。
    async fn fetch_funding_fees(
        &self,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Vec<FundingFee>, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Bill {
            bill_id: String,
            inst_id: String,
            ccy: String,
            bal_chg: String,
            ts: String,
        }
        const PAGE: usize = 100;
        let mut fees = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut path = format!(
                "/api/v5/account/bills?instType=SWAP&type=8&begin={start}&end={end}&limit={PAGE}"
            );
            if let Some(cursor) = &after {
                path.push_str(&format!("&after={cursor}"));
            }
            let page: Vec<Bill> = self.signed_get(&BILLS, &path).await?;
            let full = page.len() >= PAGE;
            after = page.last().map(|b| b.bill_id.clone());
            for b in page {
                let parse = |what: &str, raw: &str| {
                    ExchangeError::new(format!(
                        "OKX 资费账单 {} 的 {what} 非法: {raw:?}",
                        b.bill_id
                    ))
                };
                let Some(symbol) = from_okx(&b.inst_id, &self.public.quote) else {
                    continue;
                };
                fees.push(FundingFee {
                    exchange: Exchange::OKX,
                    symbol,
                    amount: Money::new(
                        b.bal_chg.parse().map_err(|_| parse("balChg", &b.bal_chg))?,
                        b.ccy.clone(),
                    ),
                    timestamp: b.ts.parse().map_err(|_| parse("ts", &b.ts))?,
                    tran_id: b.bill_id.parse().map_err(|_| parse("billId", &b.bill_id))?,
                });
            }
            if !full {
                return Ok(fees);
            }
        }
    }
}

#[cfg(test)]
mod settled_funding_tests {
    use super::*;

    #[test]
    fn keeps_window_and_sorts_ascending() {
        let body = r#"{"code":"0","msg":"","data":[
            {"fundingTime":"3000","realizedRate":"0.0003","fundingRate":"0.0009"},
            {"fundingTime":"2000","realizedRate":"-0.0002","fundingRate":"0"},
            {"fundingTime":"1000","realizedRate":"0.0001","fundingRate":"0"}]}"#;
        let got = parse_settled_funding(body, &"BTC".to_string(), 2000).unwrap();
        assert_eq!(
            got.iter().map(|f| f.settle_time).collect::<Vec<_>>(),
            vec![2000, 3000]
        );
        // 取实际结算的 realizedRate，不是 fundingRate
        assert_eq!(got[1].rate, 0.0003);
        assert!(got.iter().all(|f| f.exchange == Exchange::OKX));
    }

    #[test]
    fn api_error_and_bad_numbers_fail() {
        assert!(parse_settled_funding(
            r#"{"code":"51001","msg":"Instrument ID does not exist","data":[]}"#,
            &"X".into(),
            0
        )
        .is_err());
        let bad = r#"{"code":"0","msg":"","data":[{"fundingTime":"1","realizedRate":""}]}"#;
        assert!(parse_settled_funding(bad, &"X".into(), 0).is_err());
    }
}

#[cfg(test)]
mod equity_tests {
    use crate::domain::Money;
    use crate::exchange::okx::codec::{parse_quote_equity, BalanceDetail};

    fn detail(ccy: &str, eq: &str) -> BalanceDetail {
        BalanceDetail {
            ccy: ccy.into(),
            eq: eq.into(),
        }
    }

    #[test]
    fn equity_is_the_quote_currency_eq() {
        let details = [detail("BTC", "0.1"), detail("USDT", "1031.94")];
        assert_eq!(
            parse_quote_equity(&details, "USDT").unwrap(),
            Money::new(1031.94, "USDT")
        );
        // 没有计价币那一项 = 余额为 0（OKX 只列有余额的币）
        assert_eq!(
            parse_quote_equity(&details, "USDC").unwrap(),
            Money::new(0.0, "USDC")
        );
        assert!(parse_quote_equity(&[detail("USDT", "")], "USDT").is_err());
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;

    /// 撤单请求体：两种指认方式各只带自己的字段（字段名错了 OKX 会报参数错误）
    #[test]
    fn cancel_request_names_the_order_by_exactly_one_id() {
        let body = |order| {
            serde_json::to_value(CancelRequest {
                inst_id: "BTC-USDT-SWAP".into(),
                order,
            })
            .unwrap()
        };
        assert_eq!(
            body(OrderRef::ClOrdId("x1".into())),
            serde_json::json!({"instId": "BTC-USDT-SWAP", "clOrdId": "x1"})
        );
        assert_eq!(
            body(OrderRef::OrdId("123".into())),
            serde_json::json!({"instId": "BTC-USDT-SWAP", "ordId": "123"})
        );
    }
}

#[cfg(test)]
mod positions_tests {
    use super::*;

    /// USDT 永续、USDC 永续、币本位同时有 BTC 仓位：只有 BTC-USDT-SWAP 进 BTC 的持仓
    #[test]
    fn same_base_other_quotes_do_not_leak_into_btc() {
        let data: Vec<PositionData> = serde_json::from_str(
            r#"[{"instId":"BTC-USDT-SWAP","posSide":"net","pos":"3"},
                {"instId":"BTC-USDC-SWAP","posSide":"net","pos":"-7"},
                {"instId":"BTC-USD-SWAP","posSide":"net","pos":"11"}]"#,
        )
        .unwrap();
        let got: Vec<(Symbol, &str)> = mapped_positions(&data, "USDT")
            .map(|(s, d)| (s, d.inst_id.as_str()))
            .collect();
        assert_eq!(got, [("BTC".to_string(), "BTC-USDT-SWAP")]);
    }
}

#[cfg(test)]
mod place_reply_tests {
    use super::*;
    use crate::domain::RejectReason;

    fn rejected(r: Result<OrderId, PlaceError>) -> bool {
        matches!(r, Err(PlaceError::Rejected(_)))
    }
    fn indeterminate(r: Result<OrderId, PlaceError>) -> bool {
        matches!(r, Err(PlaceError::Indeterminate(_)))
    }

    #[test]
    fn accepted_returns_order_id() {
        let body = r#"{"code":"0","msg":"","data":[{"clOrdId":"x","ordId":"777","sCode":"0","sMsg":"Order placed"}]}"#;
        assert_eq!(parse_place_reply(200, body), Ok("777".to_string()));
    }

    /// 单笔被拒：只挂 maker 会吃单、余额不足、精度 —— 交易所明确不接
    #[test]
    fn per_order_rejections() {
        for s in [
            r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"51026","sMsg":"Order failed. Post-only order would have taken liquidity"}]}"#,
            r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"51008","sMsg":"Order failed. Insufficient USDT margin in account"}]}"#,
            r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"51121","sMsg":"Order quantity must be a multiple of the lot size."}]}"#,
        ] {
            assert!(rejected(parse_place_reply(200, s)), "{s}");
        }
    }

    #[test]
    fn reduce_only_message_keeps_its_reason() {
        let s = r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"51169","sMsg":"Order failed: reduce-only order"}]}"#;
        assert_eq!(
            parse_place_reply(200, s),
            Err(PlaceError::Rejected(RejectReason::ReduceOnlyClosed))
        );
    }

    /// 顶层被拒（没有单笔结果）：签名错、限频
    #[test]
    fn top_level_rejections() {
        for (status, s) in [
            (401, r#"{"code":"50113","msg":"Invalid Sign","data":[]}"#),
            (429, r#"{"code":"50011","msg":"Too Many Requests"}"#),
        ] {
            assert!(rejected(parse_place_reply(status, s)), "{s}");
        }
    }

    /// OKX 自己说结果未知的码（顶层或单笔）、5xx、解析不了、成功却没单笔结果 → 不确定
    #[test]
    fn unknown_outcomes() {
        for (status, s) in [
            (
                400,
                r#"{"code":"50004","msg":"API endpoint request timeout (does not mean that the request was successful or failed, please check the request result)","data":[]}"#,
            ),
            (
                503,
                r#"{"code":"50001","msg":"Service temporarily unavailable"}"#,
            ),
            (
                200,
                r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"51149","sMsg":"Order timed out. Please try again."}]}"#,
            ),
            (
                200,
                r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"50026","sMsg":"System error"}]}"#,
            ),
            (502, r#"{"code":"50000","msg":"x"}"#),
            (502, "<html>Bad Gateway</html>"),
            (200, ""),
            (200, r#"{"code":"0","msg":"","data":[]}"#),
            (
                200,
                r#"{"code":"0","msg":"","data":[{"ordId":"","sCode":"0","sMsg":""}]}"#,
            ),
        ] {
            assert!(indeterminate(parse_place_reply(status, s)), "{status} {s}");
        }
    }
}

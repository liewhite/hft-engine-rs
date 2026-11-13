//! OKX ExchangeClient 实现 (仅 REST)

use super::symbol::{from_okx, to_okx};
use crate::domain::{
    Exchange, ExchangeError, Greeks, OrderId, OrderStatus, OrderType, OrderUpdate, PlaceError, Side,
    FundingFee, Symbol, SymbolMeta, TimeInForce, Timestamp,
};
use crate::exchange::okx::codec::{parse_account_info, GreeksData, PositionData};
use crate::exchange::client::{place_send_error, AccountClient, ExchangeClient, ExchangeOrder};
pub use crate::exchange::okx::OkxCredentials;
use crate::exchange::okx::REST_BASE_URL;
use crate::domain::{Decimal, PriceStep};
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

/// OKX 交易所客户端
pub struct OkxClient {
    /// 合约目录（含每张币数 ctVal），进程内只拉一次：引擎预加载的品种元数据与本 client
    /// 折算张数用的每张币数都取自这一份，不存在两份各拉各的。
    ///
    /// [`ExchangeClient`] 的契约是**返回币本位数量** (见 [`crate::domain::Quantity`])，而 OKX
    /// REST 返回的是张数，故 client 必须自备折算能力，不能把折算责任推给调用方 —— 那样每个
    /// 调用点都得记着乘一次，漏一处就静默算错 contract_size 倍。
    instruments: tokio::sync::OnceCell<Vec<SymbolMeta>>,
    /// HTTP 客户端
    client: Client,
    /// 凭证（可选）
    credentials: Option<OkxCredentials>,
    /// REST API 基础 URL
    base_url: String,
    /// 计价币种 (e.g., "USDT")
    quote: String,
}

impl OkxClient {
    /// 创建新的 OKX 客户端
    pub fn new(quote: String, credentials: Option<OkxCredentials>) -> Result<Self, ExchangeError> {
        let client = crate::exchange::client::exchange_http_client(Exchange::OKX)?;

        Ok(Self {
            instruments: tokio::sync::OnceCell::new(),
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
            quote,
        })
    }

    /// 合约目录：首次调用时拉取，之后取缓存
    async fn instruments(&self) -> Result<&[SymbolMeta], ExchangeError> {
        self.instruments.get_or_try_init(|| self.get_all_instruments()).await.map(Vec::as_slice)
    }

    /// 取该 symbol 的合约乘数 (张 -> 币)，读合约目录
    async fn contract_size_of(&self, symbol: &Symbol) -> Result<Decimal, ExchangeError> {
        self.instruments()
            .await?
            .iter()
            .find(|m| &m.symbol == symbol)
            .map(|m| m.contract_size)
            .ok_or_else(|| ExchangeError::Other(format!("No OKX contract size for symbol {symbol}")))
    }

    /// 获取计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 获取凭证（供装配创建 OkxNode 使用）
    pub fn credentials(&self) -> Option<&OkxCredentials> {
        self.credentials.as_ref()
    }

    /// reqwest 错误转换
    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string())
    }

    /// ISO 8601 格式时间戳
    fn iso_timestamp() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    /// REST API 签名
    fn sign(&self, timestamp: &str, method: &str, path: &str, body: &str) -> Option<String> {
        let credentials = self.credentials.as_ref()?;
        let message = format!("{}{}{}{}", timestamp, method, path, body);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(credentials.secret.as_bytes()).ok()?;
        mac.update(message.as_bytes());
        let result = mac.finalize();
        Some(general_purpose::STANDARD.encode(result.into_bytes()))
    }

    /// 构建请求头
    fn build_headers(&self, sign: &str, timestamp: &str) -> Option<HeaderMap> {
        let credentials = self.credentials.as_ref()?;
        let mut headers = HeaderMap::new();
        headers.insert("OK-ACCESS-KEY", credentials.api_key.parse().ok()?);
        headers.insert("OK-ACCESS-SIGN", sign.parse().ok()?);
        headers.insert("OK-ACCESS-TIMESTAMP", timestamp.parse().ok()?);
        headers.insert("OK-ACCESS-PASSPHRASE", credentials.passphrase.parse().ok()?);
        headers.insert("Content-Type", "application/json".parse().ok()?);
        Some(headers)
    }

    /// 获取所有交易对信息 (公开接口)
    async fn get_all_instruments(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<InstrumentData>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct InstrumentData {
            inst_id: String,
            tick_sz: String,
            lot_sz: String,
            min_sz: String,
            ct_val: String,
        }

        let resp = self
            .client
            .get(format!(
                "{}/api/v5/public/instruments?instType=SWAP",
                self.base_url
            ))
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        // 剔除必须**留痕**：被剔掉的 symbol 会一路静默失效 —— 下单被 "No SymbolMeta" 拒、
        // 私有回报因无法折算张数被丢弃（持仓从此落后于交易所）、行情订阅被忽略。此前
        // 整段是 `filter_map` + `?`，一条日志都没有，三种症状看不出共同的根因。
        //
        // 这里只记录不报错：OKX 有数百个合约，个别新品种字段异常不该挡住整机启动。真正
        // 的拦截在投产期 `validate_subscriptions` —— 那里知道哪些 symbol 是**策略要用的**，
        // 缺了就拒绝投产（分工：加载处负责可见，投产处负责判定）。
        let mut metas = Vec::new();
        for d in data.data {
            // 只接入配置计价币的永续（如 BTC-USDT-SWAP），映射规则只在 okx::symbol 一处
            let Some(symbol) = from_okx(&d.inst_id, &self.quote) else {
                continue;
            };
            let positive = |raw: &str| crate::domain::parse_decimal(&raw).ok().filter(|v| *v > Decimal::ZERO);
            let parsed = (|| {
                Some(SymbolMeta {
                    exchange: Exchange::OKX,
                    symbol,
                    price_rule: Arc::new(PriceStep::new(positive(&d.tick_sz)?)),
                    size_step: positive(&d.lot_sz)?,
                    min_order_size: positive(&d.min_sz)?,
                    min_notional: 0.0,
                    contract_size: positive(&d.ct_val)?,
                })
            })();
            match parsed {
                Some(meta) => metas.push(meta),
                None => tracing::error!(
                    inst_id = %d.inst_id,
                    tick_sz = %d.tick_sz, lot_sz = %d.lot_sz,
                    min_sz = %d.min_sz, ct_val = %d.ct_val,
                    "OKX 合约元数据字段异常，该 symbol 不可交易（下单会被拒、私有成交按目录外上报）"
                ),
            }
        }

        Ok(metas)
    }

    /// 查询账户希腊值 (GET /api/v5/account/greeks)
    pub async fn fetch_greeks(&self) -> Result<Vec<Greeks>, ExchangeError> {
        let path = "/api/v5/account/greeks";
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<GreeksData>,
        }

        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        let data: Response = serde_json::from_str(&text)
            .map_err(|e| ExchangeError::Other(format!(
                "Failed to parse greeks response: {}, body: {}", e, text
            )))?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        let mut result = Vec::new();
        for item in &data.data {
            let greeks = item.to_greeks()
                .map_err(|e| ExchangeError::Other(e))?;
            result.push(greeks);
        }

        Ok(result)
    }

    /// 签名 GET，返回 `data` 数组；顶层 code 非 0 即错误。
    async fn signed_get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Vec<T>, ExchangeError> {
        #[derive(Deserialize)]
        struct Response<T> {
            code: String,
            msg: String,
            #[serde(default = "Vec::new")]
            data: Vec<T>,
        }
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;
        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        let data: Response<T> = serde_json::from_str(&text).map_err(|e| {
            ExchangeError::ParseError(format!("OKX {path}: {e} (body: {})", text.chars().take(300).collect::<String>()))
        })?;
        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }
        Ok(data.data)
    }

    /// 取一页挂单（`after` 为游标 = 上一页最后一条的 ordId，`None` 即首页）
    async fn fetch_pending_orders_page(
        &self,
        inst_id: &str,
        after: Option<&str>,
    ) -> Result<Vec<PendingOrderData>, ExchangeError> {
        let mut path = format!(
            "/api/v5/trade/orders-pending?instId={inst_id}&instType=SWAP&limit={PENDING_ORDERS_PAGE_LIMIT}"
        );
        if let Some(cursor) = after {
            path.push_str(&format!("&after={cursor}"));
        }
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", &path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;
        let data: PendingOrdersResponse = resp.json().await.map_err(Self::map_reqwest_error)?;
        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }
        Ok(data.data)
    }
}

/// OKX `orders-pending` 的单页条数上限（交易所固定值）
const PENDING_ORDERS_PAGE_LIMIT: usize = 100;
/// 分页拉取的页数上限。用尽即报错而非静默截断 —— 1 万张挂单远超任何正常策略规模，
/// 走到这里说明游标没有推进（交易所行为变化），继续循环只会挂死。
const PENDING_ORDERS_MAX_PAGES: usize = 100;

/// OKX 挂单快照的单条记录（REST `orders-pending`）
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingOrderData {
    inst_id: String,
    ord_id: String,
    cl_ord_id: Option<String>,
    side: String,
    state: String,
    /// 订单数量 (张)
    sz: String,
    /// 累计成交数量 (张)
    acc_fill_sz: String,
}

#[derive(Deserialize)]
struct PendingOrdersResponse {
    code: String,
    msg: String,
    data: Vec<PendingOrderData>,
}

/// 解析一条挂单记录，数量字段按 `contract_size` 折算为币本位。
///
/// # 一条解析不了 = 整份快照不可信 = `Err`（绝不跳过该条继续）
///
/// 挂单快照的消费方（启动期 `Manager::cancel_leftover_orders`）据「不在列表里 ⇒ 该单已终态」判定
/// "遗留挂单已撤净"。跳过一条就是谎报"这张单不存在"，带着无人看管的活单开跑；返回 `Err` 则拒绝
/// 启动，所以报错是安全的，静默少一条不是。
///
/// 已知取舍：市价单的 `px` 为空串，在此判为解析失败。市价单不会 resting，出现在挂单
/// 列表里本身就是异常，报错优于猜一个价格。
fn parse_pending_order(
    d: &PendingOrderData,
    quote: &str,
    contract_size: Decimal,
) -> Result<OrderUpdate, ExchangeError> {
    let fail = |field: &str, raw: &str| {
        ExchangeError::Other(format!(
            "OKX 挂单快照字段 {field} 无法解析（'{raw}'，ordId={}），整份快照不可信",
            d.ord_id
        ))
    };
    let symbol = from_okx(&d.inst_id, quote).ok_or_else(|| fail("instId", &d.inst_id))?;
    let side = match d.side.as_str() {
        "buy" => Side::Long,
        "sell" => Side::Short,
        other => return Err(fail("side", other)),
    };
    let status = match d.state.as_str() {
        "live" => OrderStatus::Pending,
        "partially_filled" => OrderStatus::PartiallyFilled,
        other => return Err(fail("state", other)),
    };
    let quantity = crate::domain::parse_decimal(&d.sz)
        .map(|v| v * contract_size)
        .map_err(|_| fail("sz", &d.sz))?;
    let filled_quantity = crate::domain::parse_decimal(&d.acc_fill_sz)
        .map(|v| v * contract_size)
        .map_err(|_| fail("accFillSz", &d.acc_fill_sz))?;

    Ok(OrderUpdate {
        order_id: d.ord_id.clone(),
        // 缺失就是缺失，**绝不用 ord_id 顶替**：归属识别（启动期只撤本引擎的单）靠它
        client_order_id: d.cl_ord_id.clone().filter(|s| !s.is_empty()),
        exchange: Exchange::OKX,
        symbol,
        side,
        status,
        quantity,
        filled_quantity,
    })
}

#[async_trait]
impl ExchangeClient for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        Ok(self.instruments().await?.to_vec())
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(self.instruments().await?.iter().filter(|m| symbol_set.contains(&m.symbol)).cloned().collect())
    }
}

#[async_trait]
impl AccountClient for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn cancel_order(&self, symbol: &Symbol, client_order_id: &str) -> Result<(), ExchangeError> {
        let path = "/api/v5/trade/cancel-order";
        let inst_id = to_okx(symbol, &self.quote);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct CancelRequest {
            inst_id: String,
            cl_ord_id: String,
        }

        let request = CancelRequest {
            inst_id,
            cl_ord_id: client_order_id.to_string(),
        };

        let body = serde_json::to_string(&request)?;
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "POST", path, &body)
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CancelData {
            s_code: String,
            s_msg: String,
        }

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<CancelData>,
        }

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if let Some(cancel_data) = data.data.first() {
            if cancel_data.s_code != "0" {
                return Err(ExchangeError::ApiError(
                    Exchange::OKX,
                    cancel_data.s_code.parse().unwrap_or(-1),
                    cancel_data.s_msg.clone(),
                ));
            }
            return Ok(());
        }

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        Ok(())
    }

    /// 拉取该 symbol 在 OKX 上的**全部**挂单。
    ///
    /// 完整性是这个接口的全部价值所在（见 [`parse_pending_order`] 与
    /// [`PENDING_ORDERS_MAX_PAGES`]）：分页取不全、或有一条解析不了，都返回 `Err`，
    /// 绝不返回一份"少了几张"的列表。
    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError> {
        let inst_id = to_okx(symbol, &self.quote);
        // 张 -> 币：REST 全程用张数，此处一次取到乘数，所有数量字段统一折算
        let contract_size = self.contract_size_of(symbol).await?;

        let mut updates = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..PENDING_ORDERS_MAX_PAGES {
            let page = self
                .fetch_pending_orders_page(&inst_id, after.as_deref())
                .await?;
            // 取满一页说明后面还有：OKX 按 ordId 倒序返回，游标传本页最后一条
            let page_full = page.len() >= PENDING_ORDERS_PAGE_LIMIT;
            let last_ord_id = page.last().map(|d| d.ord_id.clone());
            for d in &page {
                updates.push(parse_pending_order(d, &self.quote, contract_size)?);
            }
            match last_ord_id {
                Some(cursor) if page_full => after = Some(cursor),
                // 未取满 或 空页：已到末页
                _ => return Ok(updates),
            }
        }
        // 页数上限用尽仍未取完：宁可报错，也不返回一份被截断的"挂单全集"
        // —— 消费方会把"不在列表里"当成"该单已终态"（见 parse_pending_order）。
        Err(ExchangeError::Other(format!(
            "OKX {inst_id} 挂单分页超过 {PENDING_ORDERS_MAX_PAGES} 页仍未取完，快照不可信"
        )))
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let path = "/api/v5/trade/order";
        let inst_id = to_okx(&order.symbol, &self.quote);
        let side = side_to_okx(order.side);
        let (ord_type, price) = order_type_to_okx(&order.order_type);
        let sz = crate::domain::to_wire(order.quantity);
        let reduce_only = order.reduce_only;

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct OrderRequest {
            inst_id: String,
            td_mode: String,
            side: String,
            ord_type: String,
            sz: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            px: Option<String>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            reduce_only: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            cl_ord_id: Option<String>,
        }

        let request = OrderRequest {
            inst_id,
            td_mode: "cross".to_string(),
            side: side.to_string(),
            ord_type: ord_type.to_string(),
            sz,
            px: price,
            reduce_only,
            cl_ord_id: if order.client_order_id.is_empty() {
                None
            } else {
                Some(order.client_order_id.clone())
            },
        };

        let body = serde_json::to_string(&request).map_err(PlaceError::unsent)?;
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "POST", path, &body)
            .ok_or_else(|| PlaceError::unsent("No credentials"))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| PlaceError::unsent("Failed to build headers"))?;

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| place_send_error(Exchange::OKX, e))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| PlaceError::indeterminate(format!("OKX 读下单回复失败 (HTTP {status}): {e}")))?;
        parse_place_reply(status, &text)
    }

    /// 净值 = `account/balance` 的 `totalEq`（USD）；持仓总价值 = `account/positions`（全部
    /// 品种类型）各仓 `notionalUsd` 绝对值之和。口径见 [`parse_account_info`]。
    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct BalanceData {
            total_eq: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PositionNotional {
            pos: String,
            notional_usd: String,
        }
        let balance: Vec<BalanceData> = self.signed_get("/api/v5/account/balance").await?;
        let total_eq = balance
            .first()
            .ok_or_else(|| ExchangeError::ParseError("OKX account/balance 返回空 data".to_string()))?;
        let positions: Vec<PositionNotional> = self.signed_get("/api/v5/account/positions").await?;
        let notionals: Vec<(&str, &str)> = positions.iter().map(|p| (p.pos.as_str(), p.notional_usd.as_str())).collect();
        parse_account_info(&total_eq.total_eq, &notionals).map_err(ExchangeError::ParseError)
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        let path = "/api/v5/account/positions?instType=SWAP";
        let timestamp = Self::iso_timestamp();
        let sign = self
            .sign(&timestamp, "GET", path, "")
            .ok_or_else(|| ExchangeError::Other("No credentials".to_string()))?;
        let headers = self
            .build_headers(&sign, &timestamp)
            .ok_or_else(|| ExchangeError::Other("Failed to build headers".to_string()))?;

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<PositionData>,
        }

        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .headers(headers)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let data: Response = resp.json().await.map_err(Self::map_reqwest_error)?;

        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        let mut positions = Vec::new();
        for (symbol, d) in mapped_positions(&data.data, &self.quote) {

            // 张 -> 币：[`ExchangeClient`] 的契约是返回币本位（见 crate::domain::Quantity）。
            // 字段口径（空串=0、张数、双向持仓拒绝）全在 codec 里，不在此处重抄一份。
            let contract_size = self.contract_size_of(&symbol).await?;
            positions.push(
                d.to_position(symbol, contract_size)
                    .map_err(ExchangeError::ParseError)?,
            );
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
    data.iter().filter_map(move |d| match from_okx(&d.inst_id, quote) {
        Some(symbol) => Some((symbol, d)),
        None => {
            tracing::debug!(inst_id = %d.inst_id, "OKX 持仓不是本引擎接入的合约，跳过");
            None
        }
    })
}

/// 解析 `funding-rate-history`：只留 `fundingTime >= start_ms` 的，按时间升序。
///
/// 返回条数顶到单页上限 100 且最早一条仍晚于 `start_ms` 时报错（窗口没取全）——
/// 不会发生在 ≥1h 结算周期上，真出现说明交易所行为变了，静默少算比报错危险。
fn parse_settled_funding(
    text: &str,
    symbol: &Symbol,
    start_ms: crate::domain::Timestamp,
) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        funding_time: String,
        realized_rate: String,
    }
    #[derive(Deserialize)]
    struct Response {
        code: String,
        msg: String,
        #[serde(default)]
        data: Vec<Entry>,
    }
    let resp: Response = serde_json::from_str(text).map_err(|e| {
        ExchangeError::ParseError(format!("funding-rate-history: {e} (body: {})", text.chars().take(300).collect::<String>()))
    })?;
    if resp.code != "0" {
        return Err(map_okx_error(&resp.code, &resp.msg));
    }
    let mut out = Vec::with_capacity(resp.data.len());
    for e in &resp.data {
        let settle_time: u64 = e
            .funding_time
            .parse()
            .map_err(|_| ExchangeError::ParseError(format!("fundingTime 非法: {}", e.funding_time)))?;
        let rate: f64 = e
            .realized_rate
            .parse()
            .map_err(|_| ExchangeError::ParseError(format!("realizedRate 非法: {}", e.realized_rate)))?;
        out.push(crate::domain::SettledFunding { exchange: Exchange::OKX, symbol: symbol.clone(), rate, settle_time });
    }
    let earliest = out.iter().map(|f| f.settle_time).min();
    if out.len() >= 100 && earliest.is_some_and(|t| t > start_ms) {
        return Err(ExchangeError::Other(format!("OKX {symbol} 资金费历史一页 100 条未覆盖窗口起点")));
    }
    out.retain(|f| f.settle_time >= start_ms);
    out.sort_by_key(|f| f.settle_time);
    Ok(out)
}

/// OKX 自己说「结果未知」或系统侧故障、不能断定没受理的错误码（官方错误码表）：
/// 50001 服务暂不可用、50004 接口请求超时（"不代表请求成功或者失败，请检查请求结果"）、
/// 50013 系统繁忙、50026 系统错误、51149 下单超时。可能出现在顶层 `code`，也可能出现在单笔 `sCode`。
const OKX_UNKNOWN_STATUS_CODES: &[&str] = &["50001", "50004", "50013", "50026", "51149"];

/// 下单回复 → 三种结论。纯函数。
///
/// OKX 的失败回复常带非 2xx 状态（如 50004 是 HTTP 400），所以**先看正文、不看状态码**：
/// - 有单笔结果 `data[0]`：`sCode == "0"` 且有订单号 → 成功（没订单号 → 不确定）；`sCode` 属于「结果未知」→ 不确定；其余 → 拒绝；
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
    #[derive(Deserialize)]
    struct Response {
        code: String,
        #[serde(default)]
        msg: String,
        #[serde(default)]
        data: Vec<OrderData>,
    }
    let unknown = |code: &str| OKX_UNKNOWN_STATUS_CODES.contains(&code);
    let Ok(resp) = serde_json::from_str::<Response>(text) else {
        let body: String = text.chars().take(300).collect();
        return Err(PlaceError::indeterminate(format!("OKX 下单回复解析不了 (HTTP {status}): {body}")));
    };
    if let Some(d) = resp.data.first() {
        let detail = format!("HTTP {status}, sCode={}, sMsg={}", d.s_code, d.s_msg);
        return match d.s_code.as_str() {
            "0" if !d.ord_id.is_empty() => Ok(d.ord_id.clone()),
            "0" => Err(PlaceError::indeterminate(format!("OKX 受理了却没给订单号: {detail}"))),
            c if unknown(c) => Err(PlaceError::indeterminate(format!("OKX {detail}"))),
            _ => Err(PlaceError::rejected(detail)),
        };
    }
    let detail = format!("HTTP {status}, code={}, msg={}", resp.code, resp.msg);
    match resp.code.as_str() {
        "0" => Err(PlaceError::indeterminate(format!("OKX 下单回复没有单笔结果: {detail}"))),
        c if unknown(c) || status >= 500 => Err(PlaceError::indeterminate(format!("OKX {detail}"))),
        _ => Err(PlaceError::rejected(detail)),
    }
}

/// 错误码映射
fn map_okx_error(code: &str, msg: &str) -> ExchangeError {
    match code {
        "50013" => ExchangeError::RateLimited(Exchange::OKX, Duration::from_secs(60)),
        "51020" => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            format!("Position limit exceeded: {}", msg),
        ),
        "51121" => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            format!("Order quantity exceeded: {}", msg),
        ),
        _ => ExchangeError::ApiError(Exchange::OKX, code.parse().unwrap_or(-1), msg.to_string()),
    }
}

/// Side 转换
fn side_to_okx(side: Side) -> &'static str {
    match side {
        Side::Long => "buy",
        Side::Short => "sell",
    }
}

/// OrderType 转换
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
impl crate::exchange::SettledFundingSource for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    /// 拉取某币自 `start_ms` 起**已结算**的公共资金费率（`GET /api/v5/public/funding-rate-history`，
    /// 无需凭证），按结算时间升序。取 `realizedRate`（实际收付的费率）。
    ///
    /// 单次最多 100 条（最新的在前）；OKX 最短 1h 一结，24h 窗口最多 24 条，一页足够。
    /// 限频 10 次 / 2 秒 / IP。
    async fn fetch_settled_funding(
        &self,
        symbol: &Symbol,
        start_ms: crate::domain::Timestamp,
    ) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError> {
        let url = format!(
            "{}/api/v5/public/funding-rate-history?instId={}&limit=100",
            self.base_url,
            to_okx(symbol, &self.quote)
        );
        let resp = self.client.get(&url).send().await.map_err(Self::map_reqwest_error)?;
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;
        parse_settled_funding(&text, symbol, start_ms)
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
        assert_eq!(got.iter().map(|f| f.settle_time).collect::<Vec<_>>(), vec![2000, 3000]);
        // 取实际结算的 realizedRate，不是 fundingRate
        assert_eq!(got[1].rate, 0.0003);
        assert!(got.iter().all(|f| f.exchange == Exchange::OKX));
    }

    #[test]
    fn api_error_and_bad_numbers_fail() {
        assert!(parse_settled_funding(r#"{"code":"51001","msg":"Instrument ID does not exist","data":[]}"#, &"X".into(), 0).is_err());
        let bad = r#"{"code":"0","msg":"","data":[{"fundingTime":"1","realizedRate":""}]}"#;
        assert!(parse_settled_funding(bad, &"X".into(), 0).is_err());
    }
}

#[cfg(test)]
mod account_info_tests {
    use crate::exchange::okx::codec::parse_account_info;

    #[test]
    fn sums_absolute_position_notionals() {
        let info = parse_account_info("1000.5", &[("2", "200"), ("-1", "-150.5"), ("", ""), ("0", "")]).unwrap();
        assert_eq!(info.equity, 1000.5);
        assert_eq!(info.notional, 350.5);
    }

    #[test]
    fn unparsable_values_fail_instead_of_zero() {
        assert!(parse_account_info("", &[]).is_err());
        assert!(parse_account_info("1", &[("1", "abc")]).is_err());
        assert!(parse_account_info("1", &[("3", "")]).is_err(), "有仓却没有价值不能当 0");
    }
}

#[cfg(test)]
mod pending_orders_tests {
    use rust_decimal_macros::dec;
    use super::*;

    fn raw(cl_ord_id: Option<&str>) -> PendingOrderData {
        PendingOrderData {
            inst_id: "BTC-USDT-SWAP".to_string(),
            ord_id: "312269865356374016".to_string(),
            cl_ord_id: cl_ord_id.map(str::to_string),
            side: "buy".to_string(),
            state: "live".to_string(),
            sz: "3".to_string(),
            acc_fill_sz: "0".to_string(),
        }
    }

    /// 张 -> 币折算贯穿数量字段（ExchangeClient 的契约是币本位）
    #[test]
    fn quantities_are_converted_to_coin_unit() {
        let u = parse_pending_order(&raw(Some("x-abc")), "USDT", dec!(0.01)).unwrap();
        assert_eq!(u.symbol, "BTC");
        assert_eq!(u.side, Side::Long);
        assert_eq!(u.quantity, dec!(0.03), "sz 3 张 × 0.01 = 0.03 币");
        assert_eq!(u.client_order_id.as_deref(), Some("x-abc"));
    }

    /// **Critical 回归防线**：clOrdId 缺失/为空时必须是 `None`，绝不用 ord_id 顶替。
    ///
    /// 归属识别（启动期只撤本引擎的单）靠它：编造一个值会让别人的单被当成自己的、或反之。
    #[test]
    fn missing_client_order_id_stays_none() {
        for missing in [None, Some("")] {
            let u = parse_pending_order(&raw(missing), "USDT", dec!(1.0)).unwrap();
            assert_eq!(
                u.client_order_id, None,
                "clOrdId 缺失时被编造成了 {:?} —— 撤单复查的活单保护会失效",
                u.client_order_id
            );
        }
    }

    /// **Critical 回归防线**：任一字段解析不了 → 整份快照 `Err`，绝不跳过该条。
    ///
    /// 跳过一条 = 对"启动期撤净遗留单"与"撤单复查"谎报该单不存在（见函数文档）。
    #[test]
    fn any_unparsable_field_fails_the_whole_snapshot() {
        let cases: Vec<(&str, Box<dyn Fn(&mut PendingOrderData)>)> = vec![
            ("instId", Box::new(|d: &mut PendingOrderData| d.inst_id = "???".to_string())),
            ("side", Box::new(|d: &mut PendingOrderData| d.side = "sideways".to_string())),
            ("state", Box::new(|d: &mut PendingOrderData| d.state = "canceled".to_string())),
            ("sz", Box::new(|d: &mut PendingOrderData| d.sz = "n/a".to_string())),
        ];
        for (field, corrupt) in cases {
            let mut d = raw(Some("x-abc"));
            corrupt(&mut d);
            let err = parse_pending_order(&d, "USDT", dec!(1.0)).unwrap_err();
            assert!(
                err.to_string().contains(field),
                "{field} 解析失败应让整份快照报错（含字段名以便定位），实际: {err}"
            );
        }
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
        let got: Vec<(Symbol, &str)> =
            mapped_positions(&data, "USDT").map(|(s, d)| (s, d.inst_id.as_str())).collect();
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
        assert_eq!(parse_place_reply(200, s), Err(PlaceError::Rejected(RejectReason::ReduceOnlyClosed)));
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
            (400, r#"{"code":"50004","msg":"API endpoint request timeout (does not mean that the request was successful or failed, please check the request result)","data":[]}"#),
            (503, r#"{"code":"50001","msg":"Service temporarily unavailable"}"#),
            (200, r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"51149","sMsg":"Order timed out. Please try again."}]}"#),
            (200, r#"{"code":"1","msg":"","data":[{"ordId":"","sCode":"50026","sMsg":"System error"}]}"#),
            (502, r#"{"code":"50000","msg":"x"}"#),
            (502, "<html>Bad Gateway</html>"),
            (200, ""),
            (200, r#"{"code":"0","msg":"","data":[]}"#),
            (200, r#"{"code":"0","msg":"","data":[{"ordId":"","sCode":"0","sMsg":""}]}"#),
        ] {
            assert!(indeterminate(parse_place_reply(status, s)), "{status} {s}");
        }
    }
}

#[async_trait::async_trait]
impl crate::exchange::FundingFeeSource for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    /// 查询 `[start, end]` 内的资金费结算明细（GET /api/v5/account/bills，type=8，近 7 天内）。
    ///
    /// 金额取 `balChg`（账户余额的带符号变化：正 = 收到，负 = 付出），结算 ID 取 `billId`。
    /// 只返回本适配器计价币的永续（与品种目录同一映射），其余合约的资费不归本引擎。
    async fn fetch_funding_fees(&self, start: Timestamp, end: Timestamp) -> Result<Vec<FundingFee>, ExchangeError> {
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
            let mut path = format!("/api/v5/account/bills?instType=SWAP&type=8&begin={start}&end={end}&limit={PAGE}");
            if let Some(cursor) = &after {
                path.push_str(&format!("&after={cursor}"));
            }
            let page: Vec<Bill> = self.signed_get(&path).await?;
            let full = page.len() >= PAGE;
            after = page.last().map(|b| b.bill_id.clone());
            for b in page {
                let parse = |what: &str, raw: &str| {
                    ExchangeError::ParseError(format!("OKX 资费账单 {} 的 {what} 非法: {raw:?}", b.bill_id))
                };
                let Some(symbol) = from_okx(&b.inst_id, &self.quote) else { continue };
                fees.push(FundingFee {
                    exchange: Exchange::OKX,
                    symbol,
                    asset: b.ccy.clone(),
                    amount: b.bal_chg.parse().map_err(|_| parse("balChg", &b.bal_chg))?,
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

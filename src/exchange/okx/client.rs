//! OKX ExchangeClient 实现 (仅 REST)

use super::symbol::{from_okx, to_okx};
use crate::domain::{
    Exchange, ExchangeError, Greeks, OrderId, OrderStatus, OrderType, OrderUpdate, Side,
    Symbol, SymbolMeta, TimeInForce,
};
use crate::exchange::okx::codec::{GreeksData, PositionData};
use crate::exchange::client::{ExchangeClient, ExchangeOrder};
pub use crate::exchange::okx::OkxCredentials;
use crate::exchange::okx::REST_BASE_URL;
use crate::exchange::utils::StepFormatter;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use reqwest::Client;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

/// OKX 交易所客户端
pub struct OkxClient {
    /// symbol -> 合约乘数 (ctVal) 缓存，惰性拉取一次
    ///
    /// [`ExchangeClient`] 的契约是**返回币本位数量** (见 [`crate::domain::Quantity`])，而 OKX
    /// REST 返回的是张数，故 client 必须自备折算能力，不能把折算责任推给调用方 —— 那样每个
    /// 调用点都得记着乘一次，漏一处就静默算错 contract_size 倍。
    contract_sizes: tokio::sync::OnceCell<HashMap<Symbol, f64>>,
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
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        Ok(Self {
            contract_sizes: tokio::sync::OnceCell::new(),
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
            quote,
        })
    }

    /// 取该 symbol 的合约乘数 (张 -> 币)，首次调用时拉取并缓存全部合约信息。
    async fn contract_size_of(&self, symbol: &Symbol) -> Result<f64, ExchangeError> {
        let sizes = self
            .contract_sizes
            .get_or_try_init(|| async {
                let metas = self.get_all_instruments().await?;
                Ok::<_, ExchangeError>(
                    metas
                        .into_iter()
                        .map(|m| (m.symbol, m.contract_size))
                        .collect::<HashMap<_, _>>(),
                )
            })
            .await?;
        sizes.get(symbol).copied().ok_or_else(|| {
            ExchangeError::Other(format!("No OKX contract size for symbol {symbol}"))
        })
    }

    /// 获取计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 获取凭证（供 ManagerActor 创建 OkxActor 使用）
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
            // 只处理配置的 quote 对应的品种 (e.g., BTC-USDT-SWAP)
            if !d.inst_id.contains(&format!("-{}-SWAP", self.quote)) {
                continue;
            }
            let parsed = from_okx(&d.inst_id).and_then(|symbol| {
                let positive = |raw: &str| raw.parse::<f64>().ok().filter(|&v| v > 0.0);
                Some(SymbolMeta {
                    exchange: Exchange::OKX,
                    symbol,
                    price_formatter: Arc::new(StepFormatter::new(positive(&d.tick_sz)?)),
                    size_step: positive(&d.lot_sz)?,
                    min_order_size: positive(&d.min_sz)?,
                    contract_size: positive(&d.ct_val)?,
                })
            });
            match parsed {
                Some(meta) => metas.push(meta),
                None => tracing::error!(
                    inst_id = %d.inst_id,
                    tick_sz = %d.tick_sz, lot_sz = %d.lot_sz,
                    min_sz = %d.min_sz, ct_val = %d.ct_val,
                    "OKX 合约元数据字段异常，该 symbol 不可交易（下单会被拒、私有回报会被丢弃）"
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
/// 挂单快照的消费方都在做「不在列表里 ⇒ 该单已终态」的推断：
/// - 启动期 `ManagerActor::cancel_leftover_orders` 据此判定"遗留挂单已撤净"；
/// - 运行期撤单失败复查（[`crate::engine::live`] 的 `cancel_recheck_verdict`）据此
///   合成 Cancelled、清掉本地 pending。
///
/// 跳过一条就是对这两处谎报"这张单不存在"：轻则带着无人看管的活单开跑，重则**误清
/// 活单**让策略在它旁边重复下单、形成双重敞口。返回 `Err` 时两处都有正确的失败分支
/// （拒绝启动 / 保留 pending 待重试），所以报错是安全的，静默少一条不是。
///
/// 已知取舍：市价单的 `px` 为空串，在此判为解析失败。市价单不会 resting，出现在挂单
/// 列表里本身就是异常，报错优于猜一个价格。
fn parse_pending_order(
    d: &PendingOrderData,
    contract_size: f64,
) -> Result<OrderUpdate, ExchangeError> {
    let fail = |field: &str, raw: &str| {
        ExchangeError::Other(format!(
            "OKX 挂单快照字段 {field} 无法解析（'{raw}'，ordId={}），整份快照不可信",
            d.ord_id
        ))
    };
    let symbol = from_okx(&d.inst_id).ok_or_else(|| fail("instId", &d.inst_id))?;
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
    let quantity = d
        .sz
        .parse::<f64>()
        .map(|v| v * contract_size)
        .map_err(|_| fail("sz", &d.sz))?;

    Ok(OrderUpdate {
        order_id: d.ord_id.clone(),
        // 缺失就是缺失，**绝不用 ord_id 顶替**：撤单复查的 `Unverifiable` 安全分支判据
        // 正是 `client_order_id.is_none()` —— 编造一个非 None 值会让该分支永不触发，
        // 无法比对身份的活单被判 `Gone`、合成 Cancelled 误清本地 pending。
        client_order_id: d.cl_ord_id.clone().filter(|s| !s.is_empty()),
        exchange: Exchange::OKX,
        symbol,
        side,
        status,
        quantity,
    })
}

#[async_trait]
impl ExchangeClient for OkxClient {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        self.get_all_instruments().await
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.get_all_instruments().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all.into_iter().filter(|m| symbol_set.contains(&m.symbol)).collect())
    }

    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError> {
        let path = "/api/v5/trade/cancel-order";
        let inst_id = to_okx(symbol, &self.quote);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct CancelRequest {
            inst_id: String,
            ord_id: String,
        }

        let request = CancelRequest {
            inst_id,
            ord_id: order_id.clone(),
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
        // 无凭证 = 只接公共行情（见 ExchangeAccess），没有账户就没有挂单 —— 与
        // fetch_positions 同一口径。若在此返回 Err，`ManagerActor` 启动期会**逐 symbol**
        // 各报一次错（模拟盘跑几百个 symbol 就是几百条 warn），噪音会盖住真实告警。
        //
        // 注意：place_order / cancel_order / set_leverage 不该这样处理 —— 那些是"没凭证就
        // 真的做不到"，报错是正确行为。只有只读查询才能把"没有账户"表达成空结果。
        if self.credentials.is_none() {
            return Ok(Vec::new());
        }

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
                updates.push(parse_pending_order(d, contract_size)?);
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

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let path = "/api/v5/trade/order";
        let inst_id = to_okx(&order.symbol, &self.quote);
        let side = side_to_okx(order.side);
        let (ord_type, price) = order_type_to_okx(&order.order_type);
        let sz = order.quantity.to_string();
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
        struct OrderData {
            ord_id: String,
            s_code: String,
            s_msg: String,
        }

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<OrderData>,
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

        // 先检查 data 中的具体错误信息（更详细）
        if let Some(order_data) = data.data.first() {
            if order_data.s_code != "0" {
                let msg = format!("code={}, msg={}", order_data.s_code, order_data.s_msg);
                return Err(ExchangeError::OrderRejected(
                    Exchange::OKX,
                    crate::domain::RejectReason::classify(&msg),
                ));
            }
            return Ok(order_data.ord_id.clone());
        }

        // 如果没有 data，检查顶层错误
        if data.code != "0" {
            return Err(map_okx_error(&data.code, &data.msg));
        }

        Err(ExchangeError::OrderRejected(
            Exchange::OKX,
            crate::domain::RejectReason::Other("No order data in response".to_string()),
        ))
    }

    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        // OKX 的 equity/notional 经私有 WS 推送（AccountInfo 事件），本方法没有实现完整
        // 语义（此前 notional 硬编码 0.0）。返回编造的数字比报错危险得多 —— 下游拿它算
        // account_leverage = notional / equity 会静默失效。诚实报不支持，调用方要么走
        // WS 推送，要么在这里补齐真实实现。
        Err(ExchangeError::Other(
            "OKX 不支持 REST fetch_account_info（equity/notional 走私有 WS 推送），\
             拒绝返回编造的数字"
                .to_string(),
        ))
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        // 无凭证 = 只接公共行情（见 ExchangeAccess），没有账户可查，空仓是事实而非缺数据
        if self.credentials.is_none() {
            return Ok(Vec::new());
        }

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
        for d in &data.data {
            let Some(symbol) = from_okx(&d.inst_id) else {
                // 账户里可能有本项目未接入的合约（如交割合约），跳过而非报错
                tracing::debug!(inst_id = %d.inst_id, "OKX position 的 instId 无法识别，跳过");
                continue;
            };

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
            (ord_type, Some(price.to_string()))
        }
    }
}

#[cfg(test)]
mod pending_orders_tests {
    use super::*;

    fn raw(cl_ord_id: Option<&str>) -> PendingOrderData {
        PendingOrderData {
            inst_id: "BTC-USDT-SWAP".to_string(),
            ord_id: "312269865356374016".to_string(),
            cl_ord_id: cl_ord_id.map(str::to_string),
            side: "buy".to_string(),
            state: "live".to_string(),
            sz: "3".to_string(),
        }
    }

    /// 张 -> 币折算贯穿数量字段（ExchangeClient 的契约是币本位）
    #[test]
    fn quantities_are_converted_to_coin_unit() {
        let u = parse_pending_order(&raw(Some("x-abc")), 0.01).unwrap();
        assert_eq!(u.symbol, "BTC");
        assert_eq!(u.side, Side::Long);
        assert!((u.quantity - 0.03).abs() < 1e-12, "sz 3 张 × 0.01 = 0.03 币");
        assert_eq!(u.client_order_id.as_deref(), Some("x-abc"));
    }

    /// **Critical 回归防线**：clOrdId 缺失/为空时必须是 `None`，绝不用 ord_id 顶替。
    ///
    /// 撤单复查的 `Unverifiable` 安全分支判据正是 `client_order_id.is_none()`：编造一个
    /// 非 None 值会让该分支永不触发，无法比对身份的活单被判 `Gone` → 合成 Cancelled
    /// 误清本地 pending → 策略重复下单、双重敞口。
    #[test]
    fn missing_client_order_id_stays_none() {
        for missing in [None, Some("")] {
            let u = parse_pending_order(&raw(missing), 1.0).unwrap();
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
            let err = parse_pending_order(&d, 1.0).unwrap_err();
            assert!(
                err.to_string().contains(field),
                "{field} 解析失败应让整份快照报错（含字段名以便定位），实际: {err}"
            );
        }
    }
}

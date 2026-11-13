//! Hyperliquid ExchangeClient 实现 (REST)

use super::markets::{
    dex_of_coin, dex_offset, hl_price_rule, perp_dex_names, DexUniverse, HlAsset, HlMarkets,
};
use crate::domain::{
    AccountInfo, Exchange, ExchangeError, FundingFee, Order, OrderId, OrderType, PlaceError,
    Position, Side, Symbol, SymbolMeta, Timestamp,
};
use crate::domain::{Decimal, PriceRule};
use crate::exchange::client::{AccountClient, ExchangeClient, ExchangeOrder, OpenOrder};
use crate::exchange::hyperliquid::codec::{ClearinghouseState, MetaResponse};
use crate::exchange::hyperliquid::signing::{
    CancelResponse, OrderResponse, OrderResponseData, OrderStatus,
};
use crate::exchange::hyperliquid::transport::{
    decimal_to_wire, EnclaveSubmitter, HlCancelIntent, HlOrderIntent, HlSubmitter, HlTif,
    LocalKeySubmitter,
};
use crate::exchange::hyperliquid::{
    HyperliquidCredentials, HyperliquidSigning, IP_WEIGHT, LIMITER, REST_BASE_URL,
};
use crate::exchange::rate_limit::Admitted;
use async_trait::async_trait;
use md5::{Digest, Md5};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

/// 账户的保证金口径（`POST /info {"type":"userAbstraction"}`，启动时查一次）。
///
/// 决定净值和持仓总价值从哪里读，见 [`AccountClient::fetch_account_info`] 的实现。
/// 运行中用户若在网页上切换模式，本进程不会察觉，切换后读到的净值是否偏大**未核实**
/// （残余窗口，如实声明；切模式属人工操作，按规程应先停本进程）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HlAccountMode {
    /// 统一账户：同一抵押品（USDC）的余额与占用都在 spot clearinghouse，所有 dex 的全仓
    /// 仓位共享这一份保证金；官方文档：各 perp dex 的 user state 没有意义（本账户实测
    /// accountValue 为 0，另一有仓位的统一账户实测为各 dex 分摊的数，都不是账户净值）。
    Unified,
    /// 标准账户（`default` / `disabled`）：各 perp dex 保证金分开，净值在各自的
    /// clearinghouseState。只支持单 dex（多 dex 时「账户净值」不是一个数，杠杆口径不同）。
    Standard,
}

impl HlAccountMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "unifiedAccount" => Ok(Self::Unified),
            "default" | "disabled" => Ok(Self::Standard),
            // portfolioMargin / dexAbstraction：保证金口径未核实，不猜
            other => Err(format!(
                "HL 账户模式 {other} 未支持（只支持统一账户与标准账户）"
            )),
        }
    }
}

/// Hyperliquid 公共接口（品种目录、行情、已结算资金费率），不需要凭证。一个 client 可接多个
/// perp dex，品种目录见 [`HlMarkets`]。账户接口是 [`HyperliquidAccount`]，只有配了凭证才有。
pub struct HyperliquidClient {
    client: Client,
    base_url: String,
    /// 计价币种 (e.g., "USDC")；统一账户的净值读 spot 里这个币的余额
    quote: String,
    /// 已接入 dex 的品种目录（启动时建好，只读）
    markets: Arc<HlMarkets>,
}

impl HyperliquidClient {
    /// 建客户端：拉各 dex 的 meta 建品种目录。
    ///
    /// `dexes`：接入的 perp dex（"" = 主站，"xyz" = 股票），不能为空、不能重复。
    /// 同名品种出现在两个已接入 dex 时在这里就报错（启动失败，不带病运行）。
    pub async fn connect(quote: String, dexes: Vec<String>) -> Result<Self, ExchangeError> {
        let other = |m: String| ExchangeError::new(m);
        if dexes.is_empty() {
            return Err(other("HL 至少要接一个 dex（\"\" = 主站）".into()));
        }
        if let Some(dup) = dexes
            .iter()
            .enumerate()
            .find(|(i, d)| dexes[..*i].contains(d))
        {
            return Err(other(format!("HL dexes 配置重复: \"{}\"", dup.1)));
        }

        let client = crate::exchange::client::exchange_http_client(Exchange::Hyperliquid)?;
        let base_url = REST_BASE_URL.to_string();

        // 品种目录
        let perp_dexs: Vec<Option<serde_json::Value>> =
            info(&client, &base_url, serde_json::json!({"type": "perpDexs"})).await?;
        // 计价币的 spot token 编号：各 dex 的保证金币种必须就是它（统一账户净值只读这一个币）
        let spot_meta: serde_json::Value =
            info(&client, &base_url, serde_json::json!({"type": "spotMeta"})).await?;
        let quote_token = spot_meta["tokens"]
            .as_array()
            .and_then(|ts| {
                ts.iter()
                    .find(|t| t["name"].as_str() == Some(quote.as_str()))
            })
            .and_then(|t| t["index"].as_u64())
            .ok_or_else(|| other(format!("HL spotMeta 里没有计价币 {quote}")))?;
        let (client_ref, base_ref, perp_dexs_ref, quote_ref) =
            (&client, &base_url, &perp_dexs, &quote);
        let metas = futures_util::future::try_join_all(dexes.iter().map(|dex| async move {
            let meta: MetaResponse =
                info(client_ref, base_ref, serde_json::json!({"type": "meta", "dex": dex})).await?;
            // 保证金币种不是计价币的 dex（如以 USDT 为保证金的 cash）：净值口径对不上，拒绝
            if u64::from(meta.collateral_token) != quote_token {
                return Err(other(format!(
                    "HL dex \"{dex}\" 的保证金币种编号 {} 不是计价币 {quote_ref}（编号 {quote_token}），不能接入",
                    meta.collateral_token
                )));
            }
            Ok::<_, ExchangeError>(DexUniverse {
                dex: dex.clone(),
                offset: dex_offset(perp_dexs_ref, dex).map_err(other)?,
                universe: meta.universe,
            })
        }))
        .await?;
        let markets = Arc::new(HlMarkets::build(metas).map_err(other)?);

        Ok(Self {
            client,
            base_url,
            quote,
            markets,
        })
    }

    /// 已接入 dex 的品种目录（适配层内的 WS actor 共用同一份）
    pub(crate) fn markets(&self) -> &Arc<HlMarkets> {
        &self.markets
    }

    /// 计价币（各已接入 dex 的保证金币种，`connect` 时校验过）
    pub(crate) fn quote(&self) -> &str {
        &self.quote
    }

    /// 发送 POST /info 查询（按类型取限频额度，没有就排队）
    async fn post_info<T: for<'de> Deserialize<'de>>(
        &self,
        body: serde_json::Value,
    ) -> Result<T, ExchangeError> {
        info(&self.client, &self.base_url, body).await
    }

    /// 已接入的每个 dex 各查一次 clearinghouseState，**全部成功才返回**（任一失败整体
    /// 报错）：持仓基线和对账读数都要完整快照，少一个 dex 会被读成那个 dex 空仓。
    async fn fetch_clearinghouse_states(
        &self,
        user: &str,
    ) -> Result<Vec<(String, ClearinghouseState)>, ExchangeError> {
        futures_util::future::try_join_all(self.markets.dexes().iter().map(|dex| async move {
            let state: ClearinghouseState = self
                .post_info(
                    serde_json::json!({"type": "clearinghouseState", "user": user, "dex": dex}),
                )
                .await?;
            Ok::<_, ExchangeError>((dex.clone(), state))
        }))
        .await
    }
}

/// Hyperliquid 账户接口（下单、撤单、持仓、净值、资金费流水）。只有配了凭证才构造，持有它即
/// 「该所有账户」（见 [`AccountClient`]）：查询地址、签名通道、保证金口径三者同生同灭。
pub struct HyperliquidAccount {
    public: Arc<HyperliquidClient>,
    credentials: HyperliquidCredentials,
    submitter: Arc<dyn HlSubmitter>,
    mode: HlAccountMode,
}

impl HyperliquidAccount {
    /// 查一次账户模式、建签名通道。标准账户配了多个 dex、账户模式未支持时报错（启动失败）。
    pub async fn connect(
        public: Arc<HyperliquidClient>,
        credentials: HyperliquidCredentials,
    ) -> Result<Self, ExchangeError> {
        let other = ExchangeError::new;
        let (client, base_url, dexes) = (&public.client, &public.base_url, public.markets.dexes());
        let raw: String = info(
            client,
            base_url,
            serde_json::json!({
                "type": "userAbstraction",
                "user": credentials.wallet_address,
            }),
        )
        .await?;
        let mode = HlAccountMode::parse(&raw).map_err(other)?;
        if mode == HlAccountMode::Standard && dexes.len() > 1 {
            return Err(other(format!(
                "HL 账户是标准模式（{raw}，各 dex 保证金分开），不支持一个进程接多个 dex {dexes:?}；\
                 改成统一账户或只接一个 dex"
            )));
        }
        // 只支持主网：网络只在构造签名通道时决定一次
        let is_mainnet = true;
        let submitter: Arc<dyn HlSubmitter> = match &credentials.signing {
            HyperliquidSigning::PrivateKey(key) => Arc::new(LocalKeySubmitter::new(
                client.clone(),
                base_url.clone(),
                key,
                is_mainnet,
            )?),
            HyperliquidSigning::Signer { url } => {
                Arc::new(EnclaveSubmitter::new(client.clone(), url.clone()))
            }
        };
        Ok(Self {
            public,
            credentials,
            submitter,
            mode,
        })
    }

    /// 查询地址（主钱包；私有 WS 订阅用）
    pub(crate) fn wallet_address(&self) -> &str {
        &self.credentials.wallet_address
    }

    /// 市价单的 IOC 限价：取当前盘口对手价，往不利方向让配置的 `market_order_slippage`，
    /// 按 HL 价格精度取整。多一次 `l2Book` 查询；对延迟敏感的路径应由策略自己用 BBO
    /// 下 IOC 限价单，不走市价单。
    ///
    /// 这次查询是下单的一部分，按交易动作取额度（没有就拒单，不排队等）。
    async fn market_limit_px(
        &self,
        coin: &str,
        is_buy: bool,
        sz_decimals: u32,
    ) -> Result<String, ExchangeError> {
        let body = serde_json::json!({"type": "l2Book", "coin": coin});
        let admitted = LIMITER.action(&[IP_WEIGHT.cost(info_weight(&body))])?;
        let book: serde_json::Value =
            post_info_with(&self.public.client, &self.public.base_url, body, admitted).await?;
        let side = if is_buy { 1 } else { 0 }; // levels = [bids, asks]
        let top = book["levels"][side][0]["px"]
            .as_str()
            .and_then(|s| crate::domain::parse_decimal(s).ok())
            .filter(|p| *p > Decimal::ZERO)
            .ok_or_else(|| ExchangeError::new(format!("{coin} 盘口无对手价，市价单无法定价")))?;
        let slippage = self.credentials.market_order_slippage;
        let slippage = crate::domain::from_f64(slippage)
            .ok_or_else(|| ExchangeError::new(format!("HL 市价滑点配置非法: {slippage}")))?;
        let px = if is_buy {
            top * (Decimal::ONE + slippage)
        } else {
            top * (Decimal::ONE - slippage)
        };
        decimal_to_wire(hl_price_rule(sz_decimals).round(px))
    }

    /// 将 domain Order 转换为下单意图
    async fn order_intent(&self, order: &Order) -> Result<HlOrderIntent, ExchangeError> {
        let HlAsset {
            coin,
            asset,
            sz_decimals,
            ..
        } = self.public.markets.asset(&order.symbol)?.clone();

        let is_buy = matches!(order.side, Side::Long);

        let (limit_px, tif) = match &order.order_type {
            OrderType::Market => (
                self.market_limit_px(&coin, is_buy, sz_decimals).await?,
                HlTif::Ioc,
            ),
            OrderType::Limit { price, tif } => {
                let tif = match tif {
                    crate::domain::TimeInForce::GTC => HlTif::Gtc,
                    crate::domain::TimeInForce::IOC => HlTif::Ioc,
                    // HL 只有 Gtc / Ioc / Alo。FOK 要么全成要么不成，换成 IOC 会部分成交，
                    // 语义不同，不偷偷降级：拒单（请求没发出）
                    crate::domain::TimeInForce::FOK => {
                        return Err(ExchangeError::new(format!(
                            "HL 不支持 FOK（{}），拒单",
                            order.symbol
                        )))
                    }
                    crate::domain::TimeInForce::PostOnly => HlTif::Alo,
                };
                (decimal_to_wire(*price)?, tif)
            }
        };

        Ok(HlOrderIntent {
            coin,
            asset,
            is_buy,
            limit_px,
            sz: decimal_to_wire(order.quantity)?,
            reduce_only: order.reduce_only,
            tif,
            cloid: order.client_order_id.clone(),
        })
    }

    /// 按客户端订单号撤一张单。`Ok` 只表示交易所受理了撤单
    async fn cancel_by_cloid(&self, asset: HlAsset, cloid: String) -> Result<(), ExchangeError> {
        let submitter = &self.submitter;
        let HlAsset { coin, asset, .. } = asset;
        let admitted = LIMITER.action(&[IP_WEIGHT.cost(1)])?;
        let raw = submitter
            .cancel(
                &HlCancelIntent {
                    coin,
                    asset,
                    cloid: cloid.clone(),
                },
                admitted,
            )
            .await?;
        let response: CancelResponse = serde_json::from_value(raw.clone()).map_err(|e| {
            ExchangeError::new(format!(
                "Failed to parse cancel response: {e} (body: {raw})"
            ))
        })?;
        if response.status != "ok" {
            return Err(ExchangeError::new(format!(
                "Hyperliquid cancel rejected: {}",
                response.response.map(|v| v.to_string()).unwrap_or_default()
            )));
        }

        // 外层 ok 不代表这一笔撤掉了：statuses 里对应位置可能是 {"error": "..."}。
        // 不展开检查会把"没撤掉"当成撤掉了 —— 而调用方（启动清场、策略撤单）正是靠返回值
        // 判断要不要继续，谎报成功会让遗留挂单被当成已清理。
        let statuses = response
            .response
            .as_ref()
            .and_then(|v| v.get("data"))
            .and_then(|d| d.get("statuses"))
            .and_then(|s| s.as_array());
        if let Some(statuses) = statuses {
            for status in statuses {
                if let Some(err) = status.get("error").and_then(|e| e.as_str()) {
                    return Err(ExchangeError::new(format!(
                        "Hyperliquid cancel cloid={cloid} failed: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// 查某个 `oid` 的 `cloid`（`POST /info` `type: "orderStatus"`）。
    ///
    /// # 为什么要单独一次请求
    ///
    /// HL 的挂单列表接口（`openOrders` / `frontendOpenOrders`）**都不返回 `cloid`**，而
    /// 启动期撤遗留挂单按 cloid 撤（`AccountClient::cancel_order`）。`orderStatus` 是能按 oid
    /// 拿到 `cloid` 的接口。
    ///
    /// 响应是**双层嵌套**（2026-08 实测）：
    /// `{"status":"order","order":{"order":{...,"cloid":...},"status":...,"statusTimestamp":...}}`
    ///
    /// 返回 `None` 表示该单查不到、或它本就没有 cloid（人工经 UI 下单是 `cloid: null`）。
    async fn cloid_of(
        &self,
        wallet_address: &str,
        oid: u64,
    ) -> Result<Option<String>, ExchangeError> {
        let resp: OrderStatusResponse = self
            .public
            .post_info(serde_json::json!({
                "type": "orderStatus",
                "user": wallet_address,
                "oid": oid,
            }))
            .await?;

        Ok(resp.order.and_then(|envelope| envelope.order.cloid))
    }
}

/// `/info` 请求的 IP 权重（官方文档 Rate limits）：这几种 2，`userRole` 60，其余 20。
fn info_weight(body: &serde_json::Value) -> u32 {
    match body["type"].as_str().unwrap_or_default() {
        "l2Book"
        | "allMids"
        | "clearinghouseState"
        | "orderStatus"
        | "spotClearinghouseState"
        | "exchangeStatus" => 2,
        "userRole" => 60,
        _ => 20,
    }
}

/// 这几种 `/info` 按返回条数另加权重：每 20 条加 1（官方文档 Rate limits）
const INFO_PER_ITEM_TYPES: &[&str] = &[
    "recentTrades",
    "historicalOrders",
    "userFills",
    "userFillsByTime",
    "fundingHistory",
    "userFunding",
    "nonUserFundingUpdates",
    "twapHistory",
    "userTwapSliceFills",
    "userTwapSliceFillsByTime",
    "delegatorHistory",
    "delegatorRewards",
    "validatorStats",
];

/// POST /info 查询：先按类型取额度（没有就排队），按返回条数另加的权重收到回复后补记。
/// 构造期还没有 `Self`，故是自由函数。
async fn info<T: for<'de> Deserialize<'de>>(
    client: &Client,
    base_url: &str,
    body: serde_json::Value,
) -> Result<T, ExchangeError> {
    let admitted = LIMITER.query(&[IP_WEIGHT.cost(info_weight(&body))]).await?;
    let per_item = INFO_PER_ITEM_TYPES.contains(&body["type"].as_str().unwrap_or_default());
    let raw: serde_json::Value = post_info_with(client, base_url, body, admitted).await?;
    if per_item {
        let items = raw.as_array().map_or(0, Vec::len) as u32;
        LIMITER.record(&[IP_WEIGHT.cost(items / 20)]);
    }
    serde_json::from_value(raw)
        .map_err(|e| ExchangeError::new(format!("HL /info 回复解析失败: {e}")))
}

/// POST /info（已取到额度）
async fn post_info_with<T: for<'de> Deserialize<'de>>(
    client: &Client,
    base_url: &str,
    body: serde_json::Value,
    admitted: Admitted,
) -> Result<T, ExchangeError> {
    let resp = crate::net::send(
        client
            .post(format!("{base_url}/info"))
            .header("Content-Type", "application/json")
            .json(&body),
        admitted,
    )
    .await
    .map_err(|e| ExchangeError::transport(Exchange::Hyperliquid, e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(ExchangeError::api(
            Exchange::Hyperliquid,
            status.as_u16() as i32,
            text,
        ));
    }

    resp.json::<T>()
        .await
        .map_err(|e| ExchangeError::transport(Exchange::Hyperliquid, e.to_string()))
}

/// 一份 clearinghouseState 里属于所查 dex 的仓位（按 coin 前缀）。持仓与持仓总价值共用：
/// 若某 dex 的响应带出别的 dex 的仓位，不会被两份快照重复计入。
fn own_positions<'a>(
    dex: &'a str,
    state: &'a ClearinghouseState,
) -> impl Iterator<Item = &'a crate::exchange::hyperliquid::codec::AssetPositionWrapper> {
    state
        .asset_positions
        .iter()
        .filter(move |w| dex_of_coin(&w.position.coin) == dex)
}

fn parse_num(field: &str, v: &str) -> Result<f64, ExchangeError> {
    v.parse::<f64>()
        .map_err(|_| ExchangeError::new(format!("HL {field} 非法: {v}")))
}

/// 各 dex 的 clearinghouseState → 本引擎的持仓（纯函数，持仓基线与对账读数的唯一出处）。
///
/// - 每份只取**属于所查 dex** 的仓位（[`own_positions`]）；
/// - 某 dex 响应非空、却没有一个仓位属于它 —— coin 前缀口径变了的征兆，报错而不是返回空：
///   本函数是持仓基线的唯一来源，静默返回空会让基线变成全零、此后由 Fill 累加永不纠正；
/// - 不在品种目录里的仓位（如人工开的逐仓股票）不返回：它们映射不到内部 symbol，本引擎
///   不交易它们（与 Fill 流同一判据 [`HlMarkets::symbol_of`]）。
pub(crate) fn positions_from_states(
    markets: &HlMarkets,
    states: &[(String, ClearinghouseState)],
) -> Result<Vec<Position>, ExchangeError> {
    let mut positions = Vec::new();
    for (dex, state) in states {
        let mine: Vec<_> = own_positions(dex, state).collect();
        if mine.is_empty() && !state.asset_positions.is_empty() {
            return Err(ExchangeError::new(format!(
                "HL clearinghouseState(dex=\"{dex}\") 返回 {} 个持仓，但没有一个属于该 dex（实得: {:?}）；\
                 疑似 coin 前缀口径变了，持仓基线不能以空 Vec 蒙混过去",
                state.asset_positions.len(),
                state.asset_positions.iter().map(|w| w.position.coin.as_str()).collect::<Vec<_>>(),
            )));
        }
        for w in mine {
            let Some(symbol) = markets.symbol_of(&w.position.coin) else {
                tracing::debug!(coin = %w.position.coin, "HL 持仓不在品种目录里，不返回");
                continue;
            };
            positions.push(w.position.to_position(symbol).map_err(ExchangeError::new)?);
        }
    }
    Ok(positions)
}

/// `spotClearinghouseState` 里用得上的部分
#[derive(Debug, Deserialize)]
pub(crate) struct SpotClearinghouseState {
    balances: Vec<SpotBalance>,
}

#[derive(Debug, Deserialize)]
struct SpotBalance {
    coin: String,
    /// 余额。统一账户下**已含**全仓仓位的未实现盈亏（实测，见 [`unified_account_info`]）。
    ///
    /// 同级的 `hold`（被占用的部分，构成未核实）有意不取：净值不扣占用，
    /// 「total − hold 当可用保证金」在统一账户下也不成立（抵押品跨 dex 共享）。
    total: String,
}

/// 统一账户的净值与持仓总价值（纯函数）。
///
/// **净值 = spot 里计价币（USDC）的 `total`。** 官方文档只说「统一账户的余额和占用都在
/// spot clearinghouse state」，没写 `total` 是否含未实现盈亏；2026-11-12 实测（只读查公开地址）：
/// 两个持主站全仓大仓位的统一账户间隔 10 秒连取 7 次，`total` 随浮盈浮亏同步变动，
/// `total − Σ unrealizedPnl` 几乎不变（其中一户约 88.67 万、浮亏约 10.7 万，7 次波动 < 0.02）；
/// 另有 5 个统一账户的 `total` 与 `portfolio` 接口的账户价值相符（含浮亏 -4552 的一户）——
/// 即 `total` 已含未实现盈亏，再加一次会重复计。
/// 只算计价币：账户里的其他现货（HYPE 等）不计入，净值偏小，杠杆偏保守。
///
/// **持仓总价值 = 各已接入 dex 全部仓位的 `positionValue` 绝对值之和**。未接入 dex 不查
/// （每 2 秒查遍全部十几个 dex 会吃掉 HL 每 IP 限频的一半以上），所以口径**不含**未接入 dex
/// 的仓位（账户由策略全权托管的前提下不会有）。
/// 余额里没有计价币 = 该币余额为 0（`balances` 是全量列表；计价币是各 dex 保证金币种已在
/// `connect` 校验过）。
pub(crate) fn unified_account_info(
    spot: &SpotClearinghouseState,
    states: &[(String, ClearinghouseState)],
    quote: &str,
) -> Result<AccountInfo, ExchangeError> {
    let equity = match spot.balances.iter().find(|b| b.coin == quote) {
        Some(b) => parse_num("spot total", &b.total)?,
        None => 0.0,
    };
    let mut notional = 0.0;
    for (dex, state) in states {
        for w in own_positions(dex, state) {
            notional += parse_num("positionValue", &w.position.position_value)?.abs();
        }
    }
    Ok(AccountInfo { equity, notional })
}

/// 标准账户（只接一个 dex）的净值与持仓总价值：该 dex 的 marginSummary
fn standard_account_info(state: &ClearinghouseState) -> Result<AccountInfo, ExchangeError> {
    Ok(AccountInfo {
        equity: parse_num("accountValue", &state.margin_summary.account_value)?,
        notional: parse_num("totalNtlPos", &state.margin_summary.total_ntl_pos)?,
    })
}

/// HL `/exchange` 下单回复（已是 JSON）→ 三种结论。纯函数。
///
/// - 顶层 `status != "ok"`（如签名/钱包不存在/nonce 不对）：整个请求没被受理 → 拒绝；
/// - 单笔 `{"error": ..}`（只挂 maker 会吃单、保证金不足、价格/数量不合规、IOC 没有对手盘）→ 拒绝；
/// - `resting` / `filled` → 成功。`filled` 只表示有成交、不代表全部成交：市价单（对手价 ± 让幅
///   的 IOC）深度不够时只成交一部分、余量撤销，HL 仍回 `filled`。用户定（#692）：这算成功 ——
///   成交多少、余量已撤都会经回报流告诉策略，策略按持仓差再补；
/// - 其余（结构对不上、没有 statuses、未知状态）→ 不确定。
fn parse_place_reply(raw: serde_json::Value) -> Result<OrderId, PlaceError> {
    let unparseable = |what: &str| PlaceError::indeterminate(format!("HL 下单回复{what}: {raw}"));
    let response: OrderResponse =
        serde_json::from_value(raw.clone()).map_err(|_| unparseable("解析不了"))?;
    if response.status != "ok" {
        let msg = response.response.map(|v| v.to_string()).unwrap_or_default();
        return Err(PlaceError::rejected(format!(
            "HL status={}: {msg}",
            response.status
        )));
    }
    let data = response
        .response
        .and_then(|v| serde_json::from_value::<OrderResponseData>(v).ok())
        .and_then(|d| d.data)
        .ok_or_else(|| unparseable("没有 statuses"))?;
    let Some(first) = data.statuses.first() else {
        return Err(unparseable("statuses 为空"));
    };
    match first {
        OrderStatus::Resting(r) => Ok(r.resting.oid.to_string()),
        OrderStatus::Filled(f) => Ok(f.filled.oid.to_string()),
        // HL 不给错误码，reduce-only 单因仓位已平被拒时固定报这句（实测原文带 ". asset=N" 后缀）
        OrderStatus::Error(e)
            if e.error
                .starts_with("Reduce only order would increase position") =>
        {
            Err(PlaceError::Rejected(
                crate::domain::RejectReason::ReduceOnlyClosed,
            ))
        }
        OrderStatus::Error(e) => Err(PlaceError::rejected(format!("HL order error: {}", e.error))),
    }
}

/// `fundingHistory` 的单条记录（`premium` 不取）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingHistoryEntry {
    funding_rate: String,
    time: u64,
}

/// 费率解析失败整批报错：少一条结算会让 24h 累计静默偏小。
fn parse_funding_history(
    entries: Vec<FundingHistoryEntry>,
    symbol: &Symbol,
) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError> {
    entries
        .into_iter()
        .map(|e| {
            let rate = e.funding_rate.parse::<f64>().map_err(|_| {
                ExchangeError::new(format!("HL fundingRate 非法: {}", e.funding_rate))
            })?;
            Ok(crate::domain::SettledFunding {
                exchange: Exchange::Hyperliquid,
                symbol: symbol.clone(),
                rate,
                settle_time: e.time,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl crate::exchange::SettledFundingSource for HyperliquidClient {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    /// 拉取某币自 `start_ms` 起**已结算**的公共资金费率（`fundingHistory`，无需凭证），
    /// 按结算时间升序。HL 每小时结算一次，24h 窗口约 24 条；单次最多返回 500 条。
    /// symbol 不在已接入 dex 的品种目录里时报错。
    ///
    /// 限频：info 接口按权重计（每 IP 1200/分钟），一次约 20，别按行情频率轮询。
    async fn fetch_settled_funding(
        &self,
        symbol: &Symbol,
        start_ms: Timestamp,
    ) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError> {
        let coin = &self.markets.asset(symbol)?.coin;
        let entries: Vec<FundingHistoryEntry> = self
            .post_info(
                serde_json::json!({"type": "fundingHistory", "coin": coin, "startTime": start_ms}),
            )
            .await?;
        parse_funding_history(entries, symbol)
    }
}

#[cfg(test)]
mod funding_history_tests {
    use super::*;

    #[test]
    fn parses_public_funding_history() {
        let body = r#"[{"coin":"xyz:TSLA","fundingRate":"0.0000052306","premium":"-0.0003","time":1794470400013},
                       {"coin":"xyz:TSLA","fundingRate":"-0.0000125","premium":"0.0","time":1794474000081}]"#;
        let entries: Vec<FundingHistoryEntry> = serde_json::from_str(body).unwrap();
        let got = parse_funding_history(entries, &"TSLA".to_string()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].symbol, "TSLA");
        assert_eq!(got[1].settle_time, 1794474000081);
        assert!((got[1].rate + 0.0000125).abs() < 1e-15);
    }

    #[test]
    fn bad_rate_fails_the_whole_batch() {
        let entries = vec![FundingHistoryEntry {
            funding_rate: "nan?".into(),
            time: 1,
        }];
        assert!(parse_funding_history(entries, &"BTC".to_string()).is_err());
    }
}

#[cfg(test)]
mod place_reply_tests {
    use super::*;

    fn filled(total: &str, oid: u64) -> serde_json::Value {
        serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":[
            {"filled":{"totalSz":total,"avgPx":"100","oid":oid}}]}}})
    }

    /// 只成交一部分（市价单深度不够、限价 IOC）也是成功：余量已撤，回报流会告诉策略（#692 用户定）
    #[test]
    fn partial_fill_is_success() {
        assert_eq!(parse_place_reply(filled("0.4", 9)).unwrap(), "9");
    }

    #[test]
    fn resting_is_success() {
        let raw = serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"resting":{"oid":42}}]}}});
        assert_eq!(parse_place_reply(raw).unwrap(), "42");
    }

    /// 单笔 error 与顶层 err 都是 HL 明确不接
    #[test]
    fn explicit_rejections() {
        for raw in [
            serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":[
                {"error":"Post only order would have immediately matched, bbo was 1.0@2.0. asset=1"}]}}}),
            serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":[
                {"error":"Insufficient margin to place order. asset=1"}]}}}),
            serde_json::json!({"status":"err","response":"User or API Wallet 0x0 does not exist."}),
        ] {
            assert!(
                matches!(parse_place_reply(raw.clone()), Err(PlaceError::Rejected(_))),
                "{raw}"
            );
        }
    }

    #[test]
    fn reduce_only_rejection_keeps_its_reason() {
        let raw = serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":[
            {"error":"Reduce only order would increase position. asset=1"}]}}});
        assert_eq!(
            parse_place_reply(raw),
            Err(PlaceError::Rejected(
                crate::domain::RejectReason::ReduceOnlyClosed
            ))
        );
    }

    /// 结构对不上 / 未知状态 / 没有 statuses → 不确定
    #[test]
    fn unexpected_shapes_are_indeterminate() {
        for raw in [
            serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":[]}}}),
            serde_json::json!({"status":"ok","response":{"type":"order"}}),
            serde_json::json!({"status":"ok","response":{"type":"order","data":{"statuses":["waitingForFill"]}}}),
            serde_json::json!({"status":"ok"}),
            serde_json::json!("garbage"),
        ] {
            assert!(
                matches!(
                    parse_place_reply(raw.clone()),
                    Err(PlaceError::Indeterminate(_))
                ),
                "{raw}"
            );
        }
    }
}

/// `userFunding` 的单条记录。
///
/// 只声明用得上的字段：`hash` 有意不取（恒为全零，见 [`FundingFeeSource::fetch_funding_fees`](crate::exchange::FundingFeeSource::fetch_funding_fees)），
/// `szi` / `fundingRate` / `nSamples` 属于费率口径，不进 [`FundingFee`]。
#[derive(Debug, Deserialize)]
struct UserFundingEntry {
    time: u64,
    delta: UserFundingDelta,
}

#[derive(Debug, Deserialize)]
struct UserFundingDelta {
    #[serde(rename = "type")]
    kind: String,
    coin: String,
    usdc: String,
}

/// 把 `userFunding` 的原始记录折算成 [`FundingFee`]（纯函数，便于用真实响应做用例）。
fn parse_user_funding(
    entries: Vec<UserFundingEntry>,
    markets: &HlMarkets,
    quote: &str,
) -> Result<Vec<FundingFee>, ExchangeError> {
    let mut result = Vec::with_capacity(entries.len());
    for e in entries {
        // 该端点当前只返回 funding，但显式过滤，避免它日后混入别的 delta 类型
        if e.delta.kind != "funding" {
            continue;
        }
        // 账户级接口返回所有 dex 的记录，只留品种目录里的（与 Fill 流同一判据），
        // 否则别的 dex 的同名标的会被剥掉前缀算进来
        let Some(symbol) = markets.symbol_of(&e.delta.coin) else {
            continue;
        };
        let amount: f64 = e.delta.usdc.trim().parse().map_err(|_| {
            ExchangeError::new(format!(
                "Failed to parse Hyperliquid funding usdc '{}' for {}",
                e.delta.usdc, e.delta.coin
            ))
        })?;
        result.push(FundingFee {
            exchange: Exchange::Hyperliquid,
            symbol,
            asset: quote.to_string(),
            amount,
            timestamp: e.time,
            tran_id: funding_tran_id(e.time, &e.delta.coin),
        });
    }
    Ok(result)
}

/// 由 `(结算时刻, coin)` 派生 [`FundingFee::tran_id`]。
///
/// HL 不给资费记录任何可用的唯一 ID（`hash` 恒为全零，见 [`FundingFeeSource::fetch_funding_fees`](crate::exchange::FundingFeeSource::fetch_funding_fees)），
/// 而实测 `(time, coin)` 是唯一的 —— `time` 单独**不**唯一：同一结算时刻会同时结算多个
/// 标的（实测 311 个时刻里有 52 个是多标的）。因此 coin 必须进哈希：下游若按
/// `(exchange, tran_id)` 去重（不含 symbol），只用 time 会让同一时刻的第二个标的被当成
/// 重复记录静默丢弃。
///
/// 用 MD5 而非 `DefaultHasher`：去重键要跨进程、跨重启稳定，标准库哈希不保证这一点。
/// 截断到 64 bit 的碰撞概率在单账户资费条数量级下可忽略（生日界约 2^32 条）。
fn funding_tran_id(time_ms: u64, coin: &str) -> u64 {
    let mut hasher = Md5::new();
    hasher.update(time_ms.to_be_bytes());
    hasher.update(b":");
    hasher.update(coin.as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("MD5 digest is 16 bytes"))
}

#[async_trait]
impl ExchangeClient for HyperliquidClient {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    /// 已接入 dex 上未下架、支持全仓的品种（启动时建好的目录，不再联网）
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        Ok(self.markets.symbol_metas())
    }

    async fn fetch_symbol_meta(
        &self,
        symbols: &[Symbol],
    ) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.fetch_all_symbol_metas().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all
            .into_iter()
            .filter(|m| symbol_set.contains(&m.symbol))
            .collect())
    }
}

#[async_trait]
impl crate::exchange::FundingFeeSource for HyperliquidAccount {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    /// 拉取账户资费结算明细（`POST /info` `type: "userFunding"`）。
    ///
    /// 由通用的 `FundingFeePoller`（`crate::exchange::funding_fee_polling`）定时调用，
    /// 下游凭 `tran_id` 去重。
    ///
    /// 为什么走 REST 而不是 WS `userFundings` 订阅：去重需要稳定的 ID，WS 推送不带任何 ID，
    /// 且重连时会重推整份 snapshot；REST 这条同样不带可用 ID，但至少能由 `(time, coin)`
    /// 稳定派生，且拉取窗口可控。
    ///
    /// # 与官方文档不一致之处（以实测为准）
    ///
    /// 文档示例里每条记录带一个非零 `hash`，看起来像天然的去重键。实测 **funding 类型的
    /// 记录 `hash` 恒为全零**（同一账户 363 条记录只有这一个取值），拿它当 ID 会让所有
    /// 资费塌缩成同一条。故 `tran_id` 由 `(time, coin)` 派生，见 [`funding_tran_id`]。
    ///
    /// # 符号约定
    ///
    /// HL 的 `usdc` 已经是站在账户角度的带符号金额（实测：空头遇正费率为正 = 收到，
    /// 空头遇负费率为负 = 支付），与 [`FundingFee`] 的约定一致，不做翻转。
    async fn fetch_funding_fees(
        &self,
        start_ms: Timestamp,
        end_ms: Timestamp,
    ) -> Result<Vec<FundingFee>, ExchangeError> {
        // 带时间段的查询一次最多回 500 条：满页就从最后一条的时刻接着取。同一毫秒可能有
        // 几个币（HL 同时结算全部币），所以下一页从该时刻（含）起，重叠的几条由下游按结算 ID 去重
        const PAGE: usize = 500;
        let user = &self.credentials.wallet_address;
        let mut entries: Vec<UserFundingEntry> = Vec::new();
        let mut from = start_ms;
        loop {
            let page: Vec<UserFundingEntry> = self
                .public
                .post_info(serde_json::json!({
                    "type": "userFunding",
                    "user": user,
                    "startTime": from,
                    "endTime": end_ms,
                }))
                .await?;
            let full = page.len() >= PAGE;
            let last = page.iter().map(|e| e.time).max();
            entries.extend(page);
            match last {
                // 整页同一毫秒（不现实）时也要往前走，不原地打转
                Some(t) if full => from = t.max(from + 1),
                _ => break,
            }
        }
        parse_user_funding(entries, &self.public.markets, &self.public.quote)
    }
}

#[async_trait]
impl AccountClient for HyperliquidAccount {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    async fn cancel_order(
        &self,
        symbol: &Symbol,
        client_order_id: &str,
    ) -> Result<(), ExchangeError> {
        let asset = self.public.markets.asset(symbol)?.clone();
        self.cancel_by_cloid(asset, client_order_id.to_string())
            .await
    }

    /// 账户在**全部** perp dex 上的挂单（不只本引擎接入的 dex：统一账户下别的 dex 的挂单同样占保证金、
    /// 同样会成交）。任一请求失败整体报错。没接入的 dex 上的挂单撤不了（没有它的品种目录），
    /// 由清场复查报出、拒绝启动。
    ///
    /// `openOrders` 按 dex 查：不带 dex 参数时只回主站。
    async fn fetch_open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        let wallet = &self.credentials.wallet_address;

        #[derive(Deserialize)]
        struct Row {
            coin: String,
            oid: u64,
        }

        let perp_dexs: Vec<Option<serde_json::Value>> = self
            .public
            .post_info(serde_json::json!({"type": "perpDexs"}))
            .await?;
        let dexes = perp_dex_names(&perp_dexs).map_err(ExchangeError::new)?;
        let per_dex = futures_util::future::try_join_all(dexes.iter().map(|dex| async move {
            self.public
                .post_info::<Vec<Row>>(
                    serde_json::json!({"type": "openOrders", "user": wallet, "dex": dex}),
                )
                .await
        }))
        .await?;
        Ok(per_dex
            .into_iter()
            .flatten()
            .map(|r| OpenOrder {
                instrument: r.coin,
                order_id: r.oid.to_string(),
            })
            .collect())
    }

    /// 按 cloid 撤（签名服务只放行按 cloid 撤单）：先按 oid 查出 cloid，每张多一次 info 请求，
    /// 只在启动清场用、张数很少。没有 cloid 的（人工经 UI 下单）撤不了，报错由清场复查拒绝启动。
    async fn cancel_open_order(&self, order: &OpenOrder) -> Result<(), ExchangeError> {
        let wallet = &self.credentials.wallet_address;
        let asset = self
            .public
            .markets
            .asset_of_coin(&order.instrument)?
            .clone();
        let oid: u64 = order
            .order_id
            .parse()
            .map_err(|_| ExchangeError::new(format!("HL oid 非法: {}", order.order_id)))?;
        let cloid = self.cloid_of(wallet, oid).await?.ok_or_else(|| {
            ExchangeError::new(format!(
                "HL 挂单 {} oid={oid} 没有 cloid（不是程序下的单），撤不了",
                order.instrument
            ))
        })?;
        self.cancel_by_cloid(asset, cloid).await
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        let submitter = &self.submitter;
        let intent = self.order_intent(order).await.map_err(PlaceError::unsent)?;
        let admitted = LIMITER.action(&[IP_WEIGHT.cost(1)])?;
        let raw = submitter.place(&intent, admitted).await?;
        parse_place_reply(raw)
    }

    /// 净值与持仓总价值，口径随账户模式（启动时查定）：
    /// - 统一账户：见 [`unified_account_info`]（spot 的 USDC + 各已接入 dex 的仓位价值）；
    /// - 标准账户（只接一个 dex）：该 dex clearinghouseState 的 marginSummary。
    ///
    /// 由账户净值轮询定时调用；任一请求失败整体报错（不拿半份数据拼净值）。
    async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError> {
        let user = self.credentials.wallet_address.as_str();
        match self.mode {
            HlAccountMode::Unified => {
                let (spot, states) = tokio::try_join!(
                    self.public.post_info::<SpotClearinghouseState>(
                        serde_json::json!({"type": "spotClearinghouseState", "user": user})
                    ),
                    self.public.fetch_clearinghouse_states(user),
                )?;
                unified_account_info(&spot, &states, &self.public.quote)
            }
            HlAccountMode::Standard => {
                // connect 已保证标准账户只接一个 dex
                let states = self.public.fetch_clearinghouse_states(user).await?;
                standard_account_info(&states[0].1)
            }
        }
    }

    /// 已接入全部 dex 的完整持仓（任一 dex 查询失败整体报错，见 [`positions_from_states`]）
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError> {
        let states = self
            .public
            .fetch_clearinghouse_states(&self.credentials.wallet_address)
            .await?;
        positions_from_states(&self.public.markets, &states)
    }
}

/// `POST /info {"type":"orderStatus"}` 的响应。
///
/// **双层嵌套**（2026-08 实测）：
/// `{"status":"order","order":{"order":{...,"cloid":...},"status":...,"statusTimestamp":...}}`
/// 查不到时是 `{"status":"unknownOid"}`（没有 `order` 字段），故 `order` 为 `Option`。
///
/// 提到模块级只为让嵌套层次能被测试锁住 —— 少一层或多一层都会让 `cloid` 静默取不到，
/// 而那会让启动期撤单认不出自己的单。
#[derive(Deserialize)]
struct OrderStatusResponse {
    #[serde(default)]
    order: Option<OrderStatusEnvelope>,
}

#[derive(Deserialize)]
struct OrderStatusEnvelope {
    order: OrderStatusOrder,
}

#[derive(Deserialize)]
struct OrderStatusOrder {
    #[serde(default)]
    cloid: Option<String>,
}

#[cfg(test)]
mod order_status_tests {
    use super::*;

    /// `orderStatus` 的**真实响应**（2026-08 实测，账户地址已抹去）。
    ///
    /// 钉死双层嵌套：`order.order.cloid`。少一层或多一层都会让 cloid 静默取不到，
    /// 而那会让启动期撤单认不出自己的单、把遗留挂单留在簿上。
    const REAL_ORDER_STATUS: &str = r#"{
      "status": "order",
      "order": {
        "order": {
          "coin":"xyz:NVDA","side":"B","limitPx":"206.7","sz":"0.0","oid":339288764942,
          "timestamp":1772722406668,"triggerCondition":"N/A","isTrigger":false,
          "triggerPx":"0.0","children":[],"isPositionTpsl":false,"reduceOnly":true,
          "orderType":"Limit","origSz":"1.0","tif":"Gtc",
          "cloid":"0x3f4b6d0f9d2f452b8eed2858b58959eb"
        },
        "status": "filled",
        "statusTimestamp": 1772722406668
      }
    }"#;

    /// 查不到 oid 时的**真实响应**（实测：没有 `order` 字段）
    const REAL_UNKNOWN_OID: &str = r#"{"status":"unknownOid"}"#;

    #[test]
    fn cloid_is_extracted_from_double_nested_envelope() {
        let resp: OrderStatusResponse = serde_json::from_str(REAL_ORDER_STATUS).expect("解析");
        let cloid = resp.order.and_then(|e| e.order.cloid);
        assert_eq!(
            cloid.as_deref(),
            Some("0x3f4b6d0f9d2f452b8eed2858b58959eb"),
            "双层嵌套解析错误会让 cloid 静默丢失"
        );
    }

    #[test]
    fn unknown_oid_yields_no_cloid_instead_of_parse_error() {
        let resp: OrderStatusResponse =
            serde_json::from_str(REAL_UNKNOWN_OID).expect("unknownOid 不该导致解析失败");
        assert!(resp.order.is_none());
    }

    /// 人工经 UI 下单是 `cloid: null`：解析成 `None`，不报错
    #[test]
    fn null_cloid_is_none() {
        let raw = REAL_ORDER_STATUS.replace(
            r#""cloid":"0x3f4b6d0f9d2f452b8eed2858b58959eb""#,
            r#""cloid":null"#,
        );
        let resp: OrderStatusResponse = serde_json::from_str(&raw).expect("解析");
        assert!(resp.order.and_then(|e| e.order.cloid).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取自 `POST /info {"type":"userFunding"}` 的**真实响应**（2026-08 实测，账户地址已抹去）。
    ///
    /// 三个要点全在这段里：
    /// - `hash` 恒为全零 —— 官方文档示例给的是非零 hash，实际不能用来去重
    /// - `coin` 带 dex 前缀 —— 账户级接口不按 dex 过滤
    /// - 同一 `time` 下有两个 coin —— 只用 time 做去重键会丢记录
    const REAL_USER_FUNDING: &str = r#"[
      {"time":1775606400000,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000",
       "delta":{"type":"funding","coin":"xyz:AAPL","usdc":"0.671826","szi":"-21.0","fundingRate":"0.0000051513","nSamples":24}},
      {"time":1775606400000,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000",
       "delta":{"type":"funding","coin":"xyz:NVDA","usdc":"2.834724","szi":"-105.0","fundingRate":"0.0000051513","nSamples":24}},
      {"time":1775610000000,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000",
       "delta":{"type":"funding","coin":"xyz:AAPL","usdc":"-0.123456","szi":"-21.0","fundingRate":"-0.0000012","nSamples":24}},
      {"time":1775610000000,"hash":"0x0000000000000000000000000000000000000000000000000000000000000000",
       "delta":{"type":"funding","coin":"ETH","usdc":"9.999999","szi":"1.0","fundingRate":"0.00001","nSamples":24}}
    ]"#;

    /// 只接 `dexes` 的品种目录：主站有 ETH，xyz 有 AAPL / NVDA
    pub(crate) fn markets_of(dexes: &[&str]) -> HlMarkets {
        let info = |name: &str| -> crate::exchange::hyperliquid::codec::AssetInfo {
            serde_json::from_str(&format!(
                r#"{{"name":"{name}","szDecimals":2,"maxLeverage":10}}"#
            ))
            .unwrap()
        };
        HlMarkets::build(
            dexes
                .iter()
                .map(|d| DexUniverse {
                    dex: d.to_string(),
                    offset: if d.is_empty() { 0 } else { 110_000 },
                    universe: if d.is_empty() {
                        vec![info("ETH"), info("BTC")]
                    } else {
                        vec![info("xyz:AAPL"), info("xyz:NVDA"), info("xyz:TSLA")]
                    },
                })
                .collect(),
        )
        .unwrap()
    }

    fn parse(dexes: &[&str]) -> Vec<FundingFee> {
        let entries: Vec<UserFundingEntry> = serde_json::from_str(REAL_USER_FUNDING).unwrap();
        parse_user_funding(entries, &markets_of(dexes), "USDC").unwrap()
    }

    /// 账户级接口会把所有 perp dex 的记录一起返回，只留已接入 dex 的
    #[test]
    fn only_entries_of_connected_dexes_are_kept() {
        let fees = parse(&["xyz"]);
        assert_eq!(fees.len(), 3, "未接入的主站 ETH 那条不该进来");
        assert!(fees
            .iter()
            .all(|f| f.symbol == "AAPL" || f.symbol == "NVDA"));

        let fees = parse(&[""]);
        assert_eq!(fees.len(), 1);
        assert_eq!(fees[0].symbol, "ETH");

        // 两个都接：全收
        assert_eq!(parse(&["", "xyz"]).len(), 4);
    }

    /// dex 前缀在入 domain 时剥掉，策略侧看到的是 base symbol
    #[test]
    fn dex_prefix_is_stripped_from_symbol() {
        let fees = parse(&["xyz"]);
        assert!(fees.iter().any(|f| f.symbol == "AAPL"));
        assert!(!fees.iter().any(|f| f.symbol.contains(':')));
    }

    /// 符号沿用 HL 的账户视角：正 = 收到，负 = 支付，不翻转
    #[test]
    fn amount_sign_follows_account_perspective() {
        let fees = parse(&["xyz"]);
        let aapl_first = fees.iter().find(|f| f.timestamp == 1775606400000).unwrap();
        assert!(
            (aapl_first.amount - 0.671826).abs() < 1e-9,
            "空头遇正费率应为收到"
        );
        let aapl_second = fees.iter().find(|f| f.timestamp == 1775610000000).unwrap();
        assert!(aapl_second.amount < 0.0, "空头遇负费率应为支付");
    }

    /// 同一时刻结算的两个标的必须拿到不同的 tran_id。
    /// 下游按 (exchange, tran_id) 去重且**不含 symbol**，若 tran_id 只由 time 派生，
    /// 每个共享时刻都会有一个标的被当成重复记录静默丢弃。
    #[test]
    fn same_timestamp_different_coins_get_distinct_tran_ids() {
        let fees = parse(&["xyz"]);
        let at_first_hour: Vec<_> = fees
            .iter()
            .filter(|f| f.timestamp == 1775606400000)
            .collect();
        assert_eq!(at_first_hour.len(), 2);
        assert_ne!(at_first_hour[0].tran_id, at_first_hour[1].tran_id);
    }

    /// tran_id 必须跨进程/跨重启稳定 —— 它是 DB 去重键，变了就会重复入库
    #[test]
    fn tran_id_is_deterministic() {
        assert_eq!(
            funding_tran_id(1775606400000, "xyz:AAPL"),
            funding_tran_id(1775606400000, "xyz:AAPL")
        );
        assert_ne!(
            funding_tran_id(1775606400000, "xyz:AAPL"),
            funding_tran_id(1775606400000, "xyz:NVDA")
        );
        assert_ne!(
            funding_tran_id(1775606400000, "xyz:AAPL"),
            funding_tran_id(1775610000000, "xyz:AAPL")
        );
    }

    /// 非 funding 的 delta 类型不该混进资费流水
    #[test]
    fn non_funding_deltas_are_skipped() {
        let json = r#"[{"time":1,"delta":{"type":"deposit","coin":"xyz:AAPL","usdc":"100.0"}}]"#;
        let entries: Vec<UserFundingEntry> = serde_json::from_str(json).unwrap();
        assert!(parse_user_funding(entries, &markets_of(&["xyz"]), "USDC")
            .unwrap()
            .is_empty());
    }

    /// 官方文档示例里 usdc 带前导空格，容错掉
    #[test]
    fn leading_whitespace_in_amount_is_tolerated() {
        let json =
            r#"[{"time":1,"delta":{"type":"funding","coin":"xyz:AAPL","usdc":" -3.625312"}}]"#;
        let entries: Vec<UserFundingEntry> = serde_json::from_str(json).unwrap();
        let fees = parse_user_funding(entries, &markets_of(&["xyz"]), "USDC").unwrap();
        assert!((fees[0].amount + 3.625312).abs() < 1e-9);
    }
}

#[cfg(test)]
mod unified_account_tests {
    use super::*;
    use crate::exchange::hyperliquid::testkit::{
        self, markets, state, OWN_EMPTY_STATE, OWN_SPOT, UNIFIED_MAIN, UNIFIED_SPOT, UNIFIED_XYZ,
    };
    use rust_decimal_macros::dec;

    fn spot(raw: &str) -> SpotClearinghouseState {
        serde_json::from_str(raw).unwrap()
    }

    fn states(list: &[(&str, &str)]) -> Vec<(String, ClearinghouseState)> {
        list.iter()
            .map(|(d, raw)| (d.to_string(), state(raw)))
            .collect()
    }

    /// 真实统一账户：净值 = spot USDC total（不是各 dex 的 accountValue 之和，
    /// 也不再加未实现盈亏），持仓总价值 = 已接入各 dex 仓位价值之和
    #[test]
    fn unified_equity_is_spot_total_and_notional_sums_connected_dexes() {
        let both = states(&[("", UNIFIED_MAIN), ("xyz", UNIFIED_XYZ)]);
        let info = unified_account_info(&spot(UNIFIED_SPOT), &both, "USDC").unwrap();
        assert!(
            (info.equity - 73788.12197337).abs() < 1e-6,
            "{}",
            info.equity
        );
        // 133560 + 114345 + 306850
        assert!(
            (info.notional - 554_755.0).abs() < 1e-6,
            "{}",
            info.notional
        );

        // 只接主站：股票仓位不计入
        let main = states(&[("", UNIFIED_MAIN)]);
        let info = unified_account_info(&spot(UNIFIED_SPOT), &main, "USDC").unwrap();
        assert!((info.notional - 247_905.0).abs() < 1e-6);
    }

    /// 本策略账户的真实响应：各 dex 的 accountValue 都是 0，净值必须读成 996.96 而不是 0
    /// （读成 0 会让杠杆闸门永不放行）
    #[test]
    fn own_unified_account_reads_spot_balance_not_zero() {
        let both = states(&[("", OWN_EMPTY_STATE), ("xyz", OWN_EMPTY_STATE)]);
        let info = unified_account_info(&spot(OWN_SPOT), &both, "USDC").unwrap();
        assert!((info.equity - 996.960256).abs() < 1e-9);
        assert_eq!(info.notional, 0.0);
        // 对照：旧口径（clearinghouseState 的 marginSummary）在统一账户下是 0
        assert_eq!(standard_account_info(&both[0].1).unwrap().equity, 0.0);
    }

    #[test]
    fn account_modes() {
        assert_eq!(
            HlAccountMode::parse("unifiedAccount").unwrap(),
            HlAccountMode::Unified
        );
        assert_eq!(
            HlAccountMode::parse("default").unwrap(),
            HlAccountMode::Standard
        );
        assert_eq!(
            HlAccountMode::parse("disabled").unwrap(),
            HlAccountMode::Standard
        );
        assert!(HlAccountMode::parse("portfolioMargin").is_err());
        assert!(HlAccountMode::parse("dexAbstraction").is_err());
    }

    /// 持仓基线 = 已接入全部 dex 的仓位合并；只接一个 dex 时另一个的仓位不出现
    #[test]
    fn positions_merge_all_connected_dexes() {
        let m = markets(&["", "xyz"]);
        let got = testkit::positions(&m, &[("", UNIFIED_MAIN), ("xyz", UNIFIED_XYZ)]);
        let mut pairs: Vec<_> = got.iter().map(|p| (p.symbol.as_str(), p.size)).collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(
            pairs,
            [
                ("BCH", dec!(500)),
                ("NVDA", dec!(1900)),
                ("kPEPE", dec!(-30000000))
            ]
        );

        let m = markets(&["xyz"]);
        let got = testkit::positions(&m, &[("xyz", UNIFIED_XYZ)]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "NVDA");
    }

    /// 某 dex 的响应带出了别的 dex 的仓位：只算所查 dex 的，不会被两份快照重复计入
    #[test]
    fn a_position_is_counted_only_under_its_own_dex() {
        let m = markets(&["", "xyz"]);
        let mut main = state(UNIFIED_MAIN);
        main.asset_positions
            .extend(state(UNIFIED_XYZ).asset_positions);
        let both = vec![
            ("".to_string(), main),
            ("xyz".to_string(), state(UNIFIED_XYZ)),
        ];
        let got = positions_from_states(&m, &both).unwrap();
        assert_eq!(got.iter().filter(|p| p.symbol == "NVDA").count(), 1);
    }

    /// 某 dex 的响应非空、却没有一个仓位属于它：前缀口径变了，报错而不是返回空基线
    #[test]
    fn foreign_only_response_is_an_error_not_an_empty_baseline() {
        let m = markets(&["", "xyz"]);
        let wrong = vec![("".to_string(), state(UNIFIED_XYZ))];
        assert!(positions_from_states(&m, &wrong).is_err());
    }

    /// 已接入 dex 上不在品种目录里的仓位：不返回（映射不到内部 symbol）
    #[test]
    fn uncatalogued_position_is_not_returned() {
        let only_btc = HlMarkets::build(vec![DexUniverse {
            dex: String::new(),
            offset: 0,
            universe: vec![serde_json::from_str(
                r#"{"name":"BTC","szDecimals":5,"maxLeverage":40}"#,
            )
            .unwrap()],
        }])
        .unwrap();
        let states = vec![("".to_string(), state(UNIFIED_MAIN))];
        let got = positions_from_states(&only_btc, &states).unwrap();
        assert!(got.iter().all(|p| p.symbol == "BTC"), "{got:?}");
    }
}

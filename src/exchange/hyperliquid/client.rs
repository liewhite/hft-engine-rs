//! Hyperliquid ExchangeClient 实现 (REST)

#![allow(dead_code)]

use super::symbol::{belongs_to_dex, from_hyperliquid, to_hyperliquid};
use crate::domain::{
    now_ms, Exchange, ExchangeError, FundingFee, Order, OrderId, OrderType, Side, Symbol,
    SymbolMeta, Timestamp,
};
use md5::{Digest, Md5};
use crate::exchange::client::{ExchangeClient, ExchangeOrder};
use crate::exchange::hyperliquid::codec::{
    size_step, AssetCtx, AssetInfo, ClearinghouseState, MetaResponse,
};
use crate::exchange::utils::SignificantFiguresFormatter;
use std::sync::Arc;
use crate::exchange::hyperliquid::signing::{
    action_hash, create_signer, sign_l1_action, BulkCancelAction, BulkOrderAction, CancelResponse,
    CancelWire, ExchangeRequest, LimitOrder, OrderResponse, OrderResponseData, OrderStatus,
    OrderType as WireOrderType, OrderWire,
};
use crate::exchange::hyperliquid::{HyperliquidCredentials, REST_BASE_URL};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

/// Builder-deployed DEX 的基础 asset index 偏移量 (Hyperliquid 协议规定)
const DEX_ASSET_INDEX_BASE: u32 = 110_000;
/// 每个 DEX 之间的 asset index 间距
const DEX_ASSET_INDEX_STEP: u32 = 10_000;

/// Hyperliquid 交易所客户端
pub struct HyperliquidClient {
    /// HTTP 客户端
    client: Client,
    /// 凭证（可选）
    credentials: Option<HyperliquidCredentials>,
    /// 签名器（从 credentials 派生）
    signer: Option<PrivateKeySigner>,
    /// REST API 基础 URL
    base_url: String,
    /// 计价币种 (e.g., "USDC", "USDE")
    quote: String,
    /// Perp DEX 名称 ("" = 默认 perp DEX, "xyz" = 股票永续合约等)
    dex: String,
    /// 是否是主网
    is_mainnet: bool,
    /// Coin -> Asset Index 映射 (懒加载)
    coin_to_asset: RwLock<Option<HashMap<String, u32>>>,
}

impl HyperliquidClient {
    /// 创建新的 Hyperliquid 客户端
    pub fn new(
        quote: String,
        dex: String,
        credentials: Option<HyperliquidCredentials>,
    ) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;

        // 如果有凭证，创建签名器
        let signer = credentials
            .as_ref()
            .map(|c| create_signer(&c.private_key))
            .transpose()
            .map_err(|e| ExchangeError::Other(format!("Failed to create signer: {}", e)))?;

        Ok(Self {
            client,
            credentials,
            signer,
            base_url: REST_BASE_URL.to_string(),
            quote,
            dex,
            is_mainnet: true, // 默认主网
            coin_to_asset: RwLock::new(None),
        })
    }

    /// 获取计价币种
    pub fn quote(&self) -> &str {
        &self.quote
    }

    /// 获取 Perp DEX 名称
    pub fn dex(&self) -> &str {
        &self.dex
    }

    /// 获取凭证
    pub fn credentials(&self) -> Option<&HyperliquidCredentials> {
        self.credentials.as_ref()
    }

    /// 获取 REST API 基础 URL
    pub fn rest_base_url(&self) -> &str {
        &self.base_url
    }

    /// reqwest 错误转换
    fn map_reqwest_error(e: reqwest::Error) -> ExchangeError {
        ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string())
    }

    /// 发送 POST /info 请求
    async fn post_info<T: for<'de> Deserialize<'de>>(
        &self,
        body: serde_json::Value,
    ) -> Result<T, ExchangeError> {
        let resp = self
            .client
            .post(format!("{}/info", self.base_url))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ExchangeError::ApiError(
                Exchange::Hyperliquid,
                status.as_u16() as i32,
                text,
            ));
        }

        resp.json::<T>().await.map_err(Self::map_reqwest_error)
    }

    /// 获取所有交易对元数据
    async fn get_meta(&self) -> Result<MetaResponse, ExchangeError> {
        self.post_info(serde_json::json!({"type": "meta", "dex": self.dex}))
            .await
    }

    /// 获取元数据和资产上下文
    async fn get_meta_and_asset_ctxs(&self) -> Result<(MetaResponse, Vec<AssetCtx>), ExchangeError> {
        // metaAndAssetCtxs 返回一个二元组: [meta, [assetCtx...]]
        let resp: serde_json::Value = self
            .post_info(serde_json::json!({"type": "metaAndAssetCtxs", "dex": self.dex}))
            .await?;

        // 解析 meta
        let meta: MetaResponse = serde_json::from_value(resp[0].clone())
            .map_err(|e| ExchangeError::Other(format!("Failed to parse meta: {}", e)))?;

        // 解析 assetCtxs
        let asset_ctxs: Vec<AssetCtx> = serde_json::from_value(resp[1].clone())
            .map_err(|e| ExchangeError::Other(format!("Failed to parse assetCtxs: {}", e)))?;

        Ok((meta, asset_ctxs))
    }

    /// 获取非默认 DEX 的 asset index 偏移量
    ///
    /// 默认 perp DEX: offset = 0
    /// Builder-deployed DEXes (如 xyz): offset = DEX_ASSET_INDEX_BASE + i * DEX_ASSET_INDEX_STEP
    ///   其中 i 为该 DEX 在 perpDexs 列表中的顺序（从 0 开始，跳过 null）
    async fn get_dex_offset(&self) -> Result<u32, ExchangeError> {
        if self.dex.is_empty() {
            return Ok(0);
        }

        // 查询 perpDexs 列表
        let dexes: Vec<Option<serde_json::Value>> = self
            .post_info(serde_json::json!({"type": "perpDexs"}))
            .await?;

        // 找到我们的 DEX 在非 null 条目中的位置
        let mut non_null_idx = 0u32;
        for entry in &dexes {
            if let Some(obj) = entry {
                let name = obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| {
                        ExchangeError::Other(format!(
                            "perpDexs entry missing 'name' field: {}",
                            obj
                        ))
                    })?;
                if name == self.dex {
                    return Ok(DEX_ASSET_INDEX_BASE + non_null_idx * DEX_ASSET_INDEX_STEP);
                }
                non_null_idx += 1;
            }
        }

        Err(ExchangeError::Other(format!(
            "DEX '{}' not found in perpDexs",
            self.dex
        )))
    }

    /// 获取 coin 对应的 asset index
    async fn get_asset_index(&self, coin: &str) -> Result<u32, ExchangeError> {
        // 先检查缓存
        {
            let guard = self.coin_to_asset.read().unwrap();
            if let Some(ref map) = *guard {
                if let Some(&idx) = map.get(coin) {
                    return Ok(idx);
                }
            }
        }

        // 需要加载 meta 数据和 DEX offset
        let (meta, offset) = tokio::try_join!(self.get_meta(), self.get_dex_offset())?;

        // 构建映射（全局 asset index = offset + 数组位置）
        let mut map = HashMap::new();
        for (idx, asset) in meta.universe.iter().enumerate() {
            map.insert(asset.name.clone(), offset + idx as u32);
        }

        // 获取结果
        let result = map
            .get(coin)
            .copied()
            .ok_or_else(|| ExchangeError::Other(format!("Unknown coin: {}", coin)));

        // 更新缓存
        {
            let mut guard = self.coin_to_asset.write().unwrap();
            *guard = Some(map);
        }

        result
    }

    /// 发送 POST /exchange 请求
    async fn post_exchange<T: for<'de> Deserialize<'de>>(
        &self,
        body: &ExchangeRequest,
    ) -> Result<T, ExchangeError> {
        let resp = self
            .client
            .post(format!("{}/exchange", self.base_url))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = resp.status();
        let text = resp.text().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            return Err(ExchangeError::ApiError(
                Exchange::Hyperliquid,
                status.as_u16() as i32,
                text,
            ));
        }

        serde_json::from_str::<T>(&text).map_err(|e| {
            ExchangeError::Other(format!(
                "Failed to parse exchange response: {} (body: {})",
                e, text
            ))
        })
    }

    /// 将 domain Order 转换为 OrderWire
    async fn order_to_wire(&self, order: &Order) -> Result<OrderWire, ExchangeError> {
        let coin = to_hyperliquid(&order.symbol, &self.quote, &self.dex);
        let asset = self.get_asset_index(&coin).await?;

        let is_buy = matches!(order.side, Side::Long);

        // 提取价格和构造订单类型
        let (limit_px, order_type) = match &order.order_type {
            OrderType::Market => {
                // 市价单: 使用极端价格
                let px = if is_buy { "999999999" } else { "0.00000001" };
                (
                    px.to_string(),
                    WireOrderType::Limit(LimitOrder {
                        tif: "Ioc".to_string(),
                    }),
                )
            }
            OrderType::Limit { price, tif } => {
                let tif_str = match tif {
                    crate::domain::TimeInForce::GTC => "Gtc",
                    crate::domain::TimeInForce::IOC => "Ioc",
                    crate::domain::TimeInForce::FOK => {
                        // FOK 要求全部成交或取消，IOC 允许部分成交，语义不同
                        // Hyperliquid 不支持 FOK，记录警告
                        tracing::warn!(
                            symbol = %order.symbol,
                            "FOK not supported on Hyperliquid, using IOC (may result in partial fill)"
                        );
                        "Ioc"
                    }
                    crate::domain::TimeInForce::PostOnly => "Alo", // Add Liquidity Only
                };
                (
                    format!("{}", price),
                    WireOrderType::Limit(LimitOrder {
                        tif: tif_str.to_string(),
                    }),
                )
            }
        };

        Ok(OrderWire {
            asset,
            is_buy,
            limit_px,
            sz: format!("{}", order.quantity),
            reduce_only: order.reduce_only,
            order_type,
            cloid: if order.client_order_id.is_empty() {
                None
            } else {
                Some(order.client_order_id.clone())
            },
        })
    }

    /// 查某个 `oid` 的 `cloid`（`POST /info` `type: "orderStatus"`）。
    ///
    /// # 为什么要单独一次请求
    ///
    /// HL 的挂单列表接口（`openOrders` / `frontendOpenOrders`）**都不返回 `cloid`**，而
    /// 归属判定必须有它 —— 启动期撤掉遗留挂单时，撤错别人的单是不可接受的。`orderStatus`
    /// 是能按 oid 拿到 `cloid` 的接口。
    ///
    /// 响应是**双层嵌套**（2026-08 实测）：
    /// `{"status":"order","order":{"order":{...,"cloid":...},"status":...,"statusTimestamp":...}}`
    ///
    /// 返回 `None` 表示该单查不到、或它本就没有 cloid（人工经 UI 下单与强平等系统单都是
    /// `cloid: null`）—— 两种情况调用方都无法证明归属，应当保守地不动那张单。
    async fn cloid_of(
        &self,
        wallet_address: &str,
        oid: u64,
    ) -> Result<Option<String>, ExchangeError> {
        let resp: OrderStatusResponse = self
            .post_info(serde_json::json!({
                "type": "orderStatus",
                "user": wallet_address,
                "oid": oid,
            }))
            .await?;

        Ok(resp.order.and_then(|envelope| envelope.order.cloid))
    }

    /// 拉取账户资费结算明细（`POST /info` `type: "userFunding"`）。
    ///
    /// 与 Binance 的 `GET /fapi/v1/income?incomeType=FUNDING_FEE` 对位：两边都由各自的
    /// polling actor 定时调用，产出统一的 [`FundingFee`]，下游凭 `tran_id` 去重。
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
    pub async fn fetch_funding_fees(
        &self,
        start_ms: Timestamp,
        end_ms: Timestamp,
    ) -> Result<Vec<FundingFee>, ExchangeError> {
        let user = self
            .credentials
            .as_ref()
            .map(|c| c.wallet_address.clone())
            .ok_or_else(|| {
                ExchangeError::Other("No Hyperliquid credentials: cannot query userFunding".into())
            })?;

        let entries: Vec<UserFundingEntry> = self
            .post_info(serde_json::json!({
                "type": "userFunding",
                "user": user,
                "startTime": start_ms,
                "endTime": end_ms,
            }))
            .await?;

        parse_user_funding(entries, &self.dex, &self.quote)
    }
}

/// `userFunding` 的单条记录。
///
/// 只声明用得上的字段：`hash` 有意不取（恒为全零，见 [`HyperliquidClient::fetch_funding_fees`]），
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
    dex: &str,
    quote: &str,
) -> Result<Vec<FundingFee>, ExchangeError> {
    let mut result = Vec::with_capacity(entries.len());
    for e in entries {
        // 该端点当前只返回 funding，但显式过滤，避免它日后混入别的 delta 类型
        if e.delta.kind != "funding" {
            continue;
        }
        // 账户级接口不按 dex 过滤，必须自己筛，否则会把别的 perp dex 的同名标的算进来
        if !belongs_to_dex(&e.delta.coin, dex) {
            continue;
        }
        let amount: f64 = e.delta.usdc.trim().parse().map_err(|_| {
            ExchangeError::Other(format!(
                "Failed to parse Hyperliquid funding usdc '{}' for {}",
                e.delta.usdc, e.delta.coin
            ))
        })?;
        result.push(FundingFee {
            exchange: Exchange::Hyperliquid,
            symbol: from_hyperliquid(&e.delta.coin),
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
/// HL 不给资费记录任何可用的唯一 ID（`hash` 恒为全零，见 [`HyperliquidClient::fetch_funding_fees`]），
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

    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let meta = self.get_meta().await?;
        let has_dex = !self.dex.is_empty();

        let metas: Vec<SymbolMeta> = meta
            .universe
            .into_iter()
            .filter(|a| {
                // 过滤条件:
                // 1. 未下架
                // 2. 默认 DEX 时排除带冒号的 asset (属于其他 DEX 如 "xyz:NVDA")
                //    非默认 DEX 时不过滤冒号 (API 已按 DEX 筛选)
                // 3. 支持全仓保证金 (排除 strictIsolated 和 noCross)
                !a.is_delisted
                    && (has_dex || !a.name.contains(':'))
                    && a.supports_cross_margin()
            })
            .map(|a| asset_info_to_symbol_meta(&a))
            .collect();

        Ok(metas)
    }

    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError> {
        let all = self.fetch_all_symbol_metas().await?;
        let symbol_set: std::collections::HashSet<_> = symbols.iter().collect();
        Ok(all
            .into_iter()
            .filter(|m| symbol_set.contains(&m.symbol))
            .collect())
    }

    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| ExchangeError::Other("No credentials configured".to_string()))?;

        // `order_id` 是交易所侧的 oid（见 fetch_pending_orders / private_ws 的映射）
        let oid: u64 = order_id.parse().map_err(|_| {
            ExchangeError::ParseError(format!("Hyperliquid order_id 不是合法的 oid: {order_id}"))
        })?;
        let coin = to_hyperliquid(symbol, &self.quote, &self.dex);
        let asset = self.get_asset_index(&coin).await?;

        // 与 place_order 同一条签名链路：msgpack action hash -> EIP-712
        let action = BulkCancelAction::new(vec![CancelWire { a: asset, o: oid }]);
        let nonce = now_ms();
        let connection_id = action_hash(&action, nonce, None)
            .map_err(|e| ExchangeError::Other(format!("Cancel action hash failed: {e}")))?;
        let signature = sign_l1_action(signer, connection_id, self.is_mainnet)
            .await
            .map_err(|e| ExchangeError::Other(format!("Signing failed: {e}")))?;

        let request = ExchangeRequest {
            action: serde_json::to_value(&action)
                .map_err(|e| ExchangeError::Other(format!("Serialize cancel action failed: {e}")))?,
            nonce,
            signature: signature.to_api_format(),
            vault_address: None,
        };

        let response: CancelResponse = self.post_exchange(&request).await?;
        if response.status != "ok" {
            return Err(ExchangeError::Other(format!(
                "Hyperliquid cancel rejected: {}",
                response.response.map(|v| v.to_string()).unwrap_or_default()
            )));
        }

        // 外层 ok 不代表这一笔撤掉了：statuses 里对应位置可能是 {"error": "..."}。
        // 不展开检查会把"没撤掉"当成撤掉了 —— 而调用方（启动期/降级撤单）正是靠返回值
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
                    return Err(ExchangeError::Other(format!(
                        "Hyperliquid cancel oid={oid} failed: {err}"
                    )));
                }
            }
        }
        Ok(())
    }

    async fn fetch_pending_orders(
        &self,
        symbol: &Symbol,
    ) -> Result<Vec<crate::domain::OrderUpdate>, ExchangeError> {
        // 无凭证 = 只接公共行情，没有账户地址可查（同 fetch_positions 口径）
        let Some(credentials) = self.credentials.as_ref() else {
            return Ok(Vec::new());
        };

        /// `frontendOpenOrders` 的条目。
        ///
        /// 用 `frontendOpenOrders` 而非 `openOrders`：后者不返回 `origSz` 与 `reduceOnly`
        /// （2026-08 实测 + 官方文档）。**两者都不返回 `cloid`**，故归属信息要另外补，
        /// 见下方 `cloid_of`。
        ///
        /// 也**不能**用 `historicalOrders` 代替：它是按状态变更记录的**日志**（同一张单
        /// 每次状态变化一条），且有条数上限，无法作为"当前挂单"的权威完整列表 ——
        /// 而完整性正是对账/撤单的前提（漏一张就等于谎报干净）。
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OpenOrder {
            coin: String,
            /// `"A"` = ask/卖，`"B"` = bid/买
            side: String,
            limit_px: String,
            /// 剩余未成交量
            sz: String,
            /// 原始下单量
            orig_sz: String,
            oid: u64,
            timestamp: u64,
            #[serde(default)]
            reduce_only: bool,
        }

        let orders: Vec<OpenOrder> = self
            .post_info(serde_json::json!({
                "type": "frontendOpenOrders",
                "user": credentials.wallet_address,
                "dex": self.dex,
            }))
            .await?;

        let mut updates = Vec::new();
        for o in orders {
            // 与 fetch_positions 同理：账户级接口不按 dex 过滤，`coin` 带前缀
            if !belongs_to_dex(&o.coin, &self.dex) {
                continue;
            }
            if from_hyperliquid(&o.coin) != *symbol {
                continue;
            }
            let side = match o.side.as_str() {
                "B" => Side::Long,
                "A" => Side::Short,
                other => {
                    tracing::warn!(side = other, coin = %o.coin, "HL openOrders 未知方向，跳过");
                    continue;
                }
            };
            let orig_sz: f64 = o
                .orig_sz
                .parse()
                .map_err(|_| ExchangeError::ParseError(format!("HL origSz 非法: {}", o.orig_sz)))?;
            let remaining: f64 = o
                .sz
                .parse()
                .map_err(|_| ExchangeError::ParseError(format!("HL sz 非法: {}", o.sz)))?;
            let filled = (orig_sz - remaining).max(0.0);

            // Hyperliquid 是币本位（contract_size = 1），无需折算
            updates.push(crate::domain::OrderUpdate {
                order_id: o.oid.to_string(),
                // 挂单列表接口不带 cloid，逐单补一次。N 是**遗留挂单数**（IOC 策略下几乎
                // 恒为 0），且只在启动期发生，代价与问题规模成正比。
                client_order_id: self.cloid_of(&credentials.wallet_address, o.oid).await?,
                exchange: Exchange::Hyperliquid,
                symbol: symbol.clone(),
                side,
                status: if filled > 0.0 {
                    crate::domain::OrderStatus::PartiallyFilled { filled }
                } else {
                    crate::domain::OrderStatus::Pending
                },
                price: o.limit_px.parse().map_err(|_| {
                    ExchangeError::ParseError(format!("HL limitPx 非法: {}", o.limit_px))
                })?,
                reduce_only: o.reduce_only,
                quantity: orig_sz,
                filled_quantity: filled,
                // 快照没有"本次成交量"这一概念
                fill_sz: 0.0,
                timestamp: o.timestamp,
            });
        }

        Ok(updates)
    }

    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError> {
        // 入参已是交易所单位并已取整（见 ExchangeOrder），此处只负责组装请求
        let order = order.inner();
        // 确保有签名器
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| ExchangeError::Other("No credentials configured".to_string()))?;

        // 转换订单格式
        let order_wire = self.order_to_wire(&order).await?;

        // 构造批量下单 action
        let action = BulkOrderAction::new(vec![order_wire]);

        // 生成 nonce (当前毫秒时间戳)
        let nonce = now_ms();

        // 计算 action hash (用于签名)
        let connection_id = action_hash(&action, nonce, None)
            .map_err(|e| ExchangeError::Other(format!("Action hash failed: {}", e)))?;

        // EIP-712 签名
        let signature = sign_l1_action(signer, connection_id, self.is_mainnet)
            .await
            .map_err(|e| ExchangeError::Other(format!("Signing failed: {}", e)))?;

        // 构造请求
        let request = ExchangeRequest {
            action: serde_json::to_value(&action)
                .map_err(|e| ExchangeError::Other(format!("Serialize action failed: {}", e)))?,
            nonce,
            signature: signature.to_api_format(),
            vault_address: None,
        };

        // 发送请求
        let response: OrderResponse = self.post_exchange(&request).await?;

        // 解析响应 — 错误时 response 是字符串，成功时是 OrderResponseData 对象
        if response.status != "ok" {
            let error_msg = match response.response {
                Some(v) => v.to_string(),
                None => format!("status={}", response.status),
            };
            return Err(ExchangeError::Other(format!(
                "Order rejected: {}",
                error_msg
            )));
        }

        // 提取订单 ID
        let resp_value = response
            .response
            .ok_or_else(|| ExchangeError::Other("Empty order response".to_string()))?;
        let resp_data: OrderResponseData = serde_json::from_value(resp_value)
            .map_err(|e| ExchangeError::Other(format!("Failed to parse order response: {}", e)))?;
        let data = resp_data
            .data
            .ok_or_else(|| ExchangeError::Other("Empty order response data".to_string()))?;

        if data.statuses.is_empty() {
            return Err(ExchangeError::Other("No order status returned".to_string()));
        }

        match &data.statuses[0] {
            OrderStatus::Resting(r) => Ok(r.resting.oid.to_string()),
            OrderStatus::Filled(f) => Ok(f.filled.oid.to_string()),
            OrderStatus::Error(e) => Err(ExchangeError::Other(format!("Order error: {}", e.error))),
        }
    }

    async fn set_leverage(&self, _symbol: &Symbol, _leverage: u32) -> Result<(), ExchangeError> {
        // Hyperliquid 杠杆在下单时自动处理，此处直接返回成功
        Ok(())
    }

    async fn fetch_account_info(&self) -> Result<crate::domain::AccountInfo, ExchangeError> {
        // Hyperliquid 通过 WebSocket 推送 equity 和 notional，这里仅实现 trait
        // 实际使用中不会调用此方法
        Ok(crate::domain::AccountInfo {
            equity: 0.0,
            notional: 0.0,
        })
    }

    async fn fetch_positions(&self) -> Result<Vec<crate::domain::Position>, ExchangeError> {
        // 无凭证 = 只接公共行情（见 ExchangeAccess），没有账户地址可查
        let Some(credentials) = self.credentials.as_ref() else {
            return Ok(Vec::new());
        };

        let state: ClearinghouseState = self
            .post_info(serde_json::json!({
                "type": "clearinghouseState",
                "user": credentials.wallet_address,
                "dex": self.dex,
            }))
            .await?;

        let mut positions = Vec::new();
        for wrapper in &state.asset_positions {
            // **必须按 dex 过滤**：账户级接口不区分 dex，`coin` 带前缀（如 `xyz:AAPL`）。
            // 漏掉这一步会把股票永续的持仓算进默认 perp DEX 的账本——与 fetch_funding_fees
            // 踩过的是同一个坑（见本文件测试里的真实响应样例）。
            if !belongs_to_dex(&wrapper.position.coin, &self.dex) {
                continue;
            }
            let position = wrapper
                .position
                .to_position()
                .map_err(ExchangeError::ParseError)?;
            positions.push(position);
        }

        // 守卫「响应非空、却被过滤得一个不剩」：这是 dex 前缀口径与预期不符的征兆
        // （例如请求里已带 dex，响应便不再给 coin 加前缀）。
        //
        // 之所以要显式报出来：本方法是持仓基线的**唯一**来源，静默返回空 Vec 会让基线变成
        // 全零、且此后由 Fill 累加、永不纠正 —— 正是本项目刚修掉的那类静默错账。宁可刺眼。
        if positions.is_empty() && !state.asset_positions.is_empty() {
            return Err(ExchangeError::ParseError(format!(
                "Hyperliquid clearinghouseState 返回 {} 个持仓，但按 dex=\"{}\" 过滤后一个不剩；\
                 疑似 coin 前缀口径与预期不符（实得: {:?}）。持仓基线不能以空 Vec 蒙混过去。",
                state.asset_positions.len(),
                self.dex,
                state
                    .asset_positions
                    .iter()
                    .map(|w| w.position.coin.as_str())
                    .collect::<Vec<_>>(),
            )));
        }

        Ok(positions)
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

/// 将 AssetInfo 转换为 SymbolMeta
fn asset_info_to_symbol_meta(info: &AssetInfo) -> SymbolMeta {
    let symbol = from_hyperliquid(&info.name);
    let sz_decimals = info.sz_decimals.max(0) as u32;

    SymbolMeta {
        exchange: Exchange::Hyperliquid,
        symbol,
        price_formatter: Arc::new(SignificantFiguresFormatter::new(sz_decimals)),
        size_step: size_step(info.sz_decimals),
        min_order_size: size_step(info.sz_decimals), // 最小下单量为一个精度单位
        contract_size: 1.0, // Hyperliquid 是币本位，合约乘数为 1
    }
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
        // 该 cloid 是本引擎格式，归属判定应认领它
        assert!(Exchange::Hyperliquid.owns_cli_order_id(&cloid.unwrap()));
    }

    #[test]
    fn unknown_oid_yields_no_cloid_instead_of_parse_error() {
        let resp: OrderStatusResponse = serde_json::from_str(REAL_UNKNOWN_OID)
            .expect("unknownOid 不该导致解析失败");
        assert!(resp.order.is_none());
    }

    /// 人工经 UI 下单 / 强平等系统单是 `cloid: null` —— 无法证明归属，不该被认领
    #[test]
    fn null_cloid_is_not_owned() {
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

    fn parse(dex: &str) -> Vec<FundingFee> {
        let entries: Vec<UserFundingEntry> = serde_json::from_str(REAL_USER_FUNDING).unwrap();
        parse_user_funding(entries, dex, "USDC").unwrap()
    }

    /// 账户级接口会把所有 perp dex 的记录一起返回，必须只留本 dex 的
    #[test]
    fn only_entries_of_the_configured_dex_are_kept() {
        let fees = parse("xyz");
        assert_eq!(fees.len(), 3, "默认 dex 的 ETH 那条不该进来");
        assert!(fees.iter().all(|f| f.symbol == "AAPL" || f.symbol == "NVDA"));

        // 反过来：接默认 dex 时只应拿到 ETH
        let fees = parse("");
        assert_eq!(fees.len(), 1);
        assert_eq!(fees[0].symbol, "ETH");
    }

    /// dex 前缀在入 domain 时剥掉，策略侧看到的是 base symbol
    #[test]
    fn dex_prefix_is_stripped_from_symbol() {
        let fees = parse("xyz");
        assert!(fees.iter().any(|f| f.symbol == "AAPL"));
        assert!(!fees.iter().any(|f| f.symbol.contains(':')));
    }

    /// 符号沿用 HL 的账户视角：正 = 收到，负 = 支付，不翻转
    #[test]
    fn amount_sign_follows_account_perspective() {
        let fees = parse("xyz");
        let aapl_first = fees.iter().find(|f| f.timestamp == 1775606400000).unwrap();
        assert!((aapl_first.amount - 0.671826).abs() < 1e-9, "空头遇正费率应为收到");
        let aapl_second = fees.iter().find(|f| f.timestamp == 1775610000000).unwrap();
        assert!(aapl_second.amount < 0.0, "空头遇负费率应为支付");
    }

    /// 同一时刻结算的两个标的必须拿到不同的 tran_id。
    /// 下游按 (exchange, tran_id) 去重且**不含 symbol**，若 tran_id 只由 time 派生，
    /// 每个共享时刻都会有一个标的被当成重复记录静默丢弃。
    #[test]
    fn same_timestamp_different_coins_get_distinct_tran_ids() {
        let fees = parse("xyz");
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
        assert!(parse_user_funding(entries, "xyz", "USDC").unwrap().is_empty());
    }

    /// 官方文档示例里 usdc 带前导空格，容错掉
    #[test]
    fn leading_whitespace_in_amount_is_tolerated() {
        let json = r#"[{"time":1,"delta":{"type":"funding","coin":"xyz:AAPL","usdc":" -3.625312"}}]"#;
        let entries: Vec<UserFundingEntry> = serde_json::from_str(json).unwrap();
        let fees = parse_user_funding(entries, "xyz", "USDC").unwrap();
        assert!((fees[0].amount + 3.625312).abs() < 1e-9);
    }
}

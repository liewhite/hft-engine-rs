//! Hyperliquid ExchangeClient 实现 (REST)

#![allow(dead_code)]

use super::symbol::{from_hyperliquid, to_hyperliquid};
use crate::domain::{now_ms, Exchange, ExchangeError, Order, OrderId, OrderType, Side, Symbol, SymbolMeta};
use crate::exchange::client::ExchangeClient;
use crate::exchange::hyperliquid::codec::{size_step, AssetCtx, AssetInfo, MetaResponse};
use crate::exchange::utils::SignificantFiguresFormatter;
use std::sync::Arc;
use crate::exchange::hyperliquid::signing::{
    action_hash, create_signer, sign_l1_action, BulkOrderAction, ExchangeRequest, LimitOrder,
    OrderResponse, OrderResponseData, OrderStatus, OrderType as WireOrderType, OrderWire,
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
    pub fn new(credentials: Option<HyperliquidCredentials>) -> Result<Self, ExchangeError> {
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

        let quote = credentials
            .as_ref()
            .map(|c| c.quote.clone())
            .unwrap_or_else(|| "USDC".to_string());

        let dex = credentials
            .as_ref()
            .map(|c| c.dex.clone())
            .unwrap_or_default();

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

    async fn cancel_order(&self, _symbol: &Symbol, _order_id: &OrderId) -> Result<(), ExchangeError> {
        Err(ExchangeError::Other("Hyperliquid cancel_order not implemented".to_string()))
    }

    async fn fetch_pending_orders(&self, _symbol: &Symbol) -> Result<Vec<crate::domain::OrderUpdate>, ExchangeError> {
        Ok(vec![])
    }

    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError> {
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

    async fn fetch_account_info(&self) -> Result<crate::exchange::AccountInfo, ExchangeError> {
        // Hyperliquid 通过 WebSocket 推送 equity 和 notional，这里仅实现 trait
        // 实际使用中不会调用此方法
        Ok(crate::exchange::AccountInfo {
            equity: 0.0,
            notional: 0.0,
        })
    }
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

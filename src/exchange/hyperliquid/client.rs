//! Hyperliquid ExchangeClient 实现 (REST)

#![allow(dead_code)]

use crate::domain::{now_ms, Exchange, ExchangeError, Order, OrderId, OrderType, Side, Symbol, SymbolMeta};
use crate::exchange::client::ExchangeClient;
use crate::exchange::hyperliquid::codec::{price_step, size_step, AssetCtx, AssetInfo, MetaResponse};
use crate::exchange::hyperliquid::signing::{
    action_hash, create_signer, sign_l1_action, BulkOrderAction, ExchangeRequest, LimitOrder,
    OrderResponse, OrderStatus, OrderType as WireOrderType, OrderWire,
};
use crate::exchange::hyperliquid::{HyperliquidCredentials, REST_BASE_URL};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

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

        Ok(Self {
            client,
            credentials,
            signer,
            base_url: REST_BASE_URL.to_string(),
            is_mainnet: true, // 默认主网
            coin_to_asset: RwLock::new(None),
        })
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
        self.post_info(serde_json::json!({"type": "meta"})).await
    }

    /// 获取元数据和资产上下文
    async fn get_meta_and_asset_ctxs(&self) -> Result<(MetaResponse, Vec<AssetCtx>), ExchangeError> {
        // metaAndAssetCtxs 返回一个二元组: [meta, [assetCtx...]]
        let resp: serde_json::Value = self
            .post_info(serde_json::json!({"type": "metaAndAssetCtxs"}))
            .await?;

        // 解析 meta
        let meta: MetaResponse = serde_json::from_value(resp[0].clone())
            .map_err(|e| ExchangeError::Other(format!("Failed to parse meta: {}", e)))?;

        // 解析 assetCtxs
        let asset_ctxs: Vec<AssetCtx> = serde_json::from_value(resp[1].clone())
            .map_err(|e| ExchangeError::Other(format!("Failed to parse assetCtxs: {}", e)))?;

        Ok((meta, asset_ctxs))
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

        // 需要加载 meta 数据
        let meta = self.get_meta().await?;

        // 构建映射
        let mut map = HashMap::new();
        for (idx, asset) in meta.universe.iter().enumerate() {
            map.insert(asset.name.clone(), idx as u32);
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

    /// 将 domain Order 转换为 OrderWire
    async fn order_to_wire(&self, order: &Order) -> Result<OrderWire, ExchangeError> {
        let coin = order.symbol.to_hyperliquid();
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

        let metas: Vec<SymbolMeta> = meta
            .universe
            .into_iter()
            .filter(|a| !a.is_delisted)
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

        // 解析响应
        if response.status != "ok" {
            return Err(ExchangeError::Other(format!(
                "Order rejected: status={}",
                response.status
            )));
        }

        // 提取订单 ID
        let data = response
            .response
            .and_then(|r| r.data)
            .ok_or_else(|| ExchangeError::Other("Empty order response".to_string()))?;

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

    async fn fetch_equity(&self) -> Result<f64, ExchangeError> {
        // 获取账户净值需要地址，如果没有凭证则报错
        let wallet_address = self
            .credentials
            .as_ref()
            .map(|c| c.wallet_address.clone())
            .ok_or_else(|| ExchangeError::Other("No wallet address configured".to_string()))?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ClearinghouseState {
            margin_summary: MarginSummary,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MarginSummary {
            account_value: String,
        }

        let state: ClearinghouseState = self
            .post_info(serde_json::json!({
                "type": "clearinghouseState",
                "user": wallet_address
            }))
            .await?;

        let equity: f64 = state
            .margin_summary
            .account_value
            .parse()
            .map_err(|_| ExchangeError::Other("Failed to parse account value".to_string()))?;

        Ok(equity)
    }
}

/// 将 AssetInfo 转换为 SymbolMeta
fn asset_info_to_symbol_meta(info: &AssetInfo) -> SymbolMeta {
    let symbol = Symbol::from_hyperliquid(&info.name);

    SymbolMeta {
        exchange: Exchange::Hyperliquid,
        symbol,
        price_step: price_step(),
        size_step: size_step(info.sz_decimals),
        min_order_size: size_step(info.sz_decimals), // 最小下单量为一个精度单位
        contract_size: 1.0, // Hyperliquid 是币本位，合约乘数为 1
    }
}

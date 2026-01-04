//! Hyperliquid ExchangeClient 实现 (REST)
//!
//! 注意: 下单功能需要 EIP-712 签名，当前仅实现只读功能

#![allow(dead_code)]

use crate::domain::{Exchange, ExchangeError, Order, OrderId, Symbol, SymbolMeta};
use crate::exchange::client::ExchangeClient;
use crate::exchange::hyperliquid::codec::{price_step, size_step, AssetCtx, AssetInfo, MetaResponse};
use crate::exchange::hyperliquid::{HyperliquidCredentials, REST_BASE_URL};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

/// Hyperliquid 交易所客户端
pub struct HyperliquidClient {
    /// HTTP 客户端
    client: Client,
    /// 凭证（可选）
    credentials: Option<HyperliquidCredentials>,
    /// REST API 基础 URL
    base_url: String,
}

impl HyperliquidClient {
    /// 创建新的 Hyperliquid 客户端
    pub fn new(credentials: Option<HyperliquidCredentials>) -> Result<Self, ExchangeError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;

        Ok(Self {
            client,
            credentials,
            base_url: REST_BASE_URL.to_string(),
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

    async fn place_order(&self, _order: Order) -> Result<OrderId, ExchangeError> {
        // 下单需要 EIP-712 签名，暂未实现
        Err(ExchangeError::Other(
            "Hyperliquid order placement requires EIP-712 signing (not yet implemented)".to_string(),
        ))
    }

    async fn set_leverage(&self, _symbol: &Symbol, _leverage: u32) -> Result<(), ExchangeError> {
        // 设置杠杆需要 EIP-712 签名，暂未实现
        Err(ExchangeError::Other(
            "Hyperliquid leverage setting requires EIP-712 signing (not yet implemented)".to_string(),
        ))
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

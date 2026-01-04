//! Hyperliquid 数据编解码
//!
//! 解析 Hyperliquid REST API 和 WebSocket 消息

#![allow(dead_code)]

use crate::domain::{now_ms, Exchange, FundingRate, Symbol, BBO};
use serde::Deserialize;
use std::str::FromStr;

// ============================================================================
// REST API 响应结构
// ============================================================================

/// Meta 响应 (交易对元数据)
#[derive(Debug, Deserialize)]
pub struct MetaResponse {
    pub universe: Vec<AssetInfo>,
}

/// 单个资产信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    /// 币种名 (e.g., "BTC", "ETH")
    pub name: String,
    /// 数量小数位数
    pub sz_decimals: i32,
    /// 最大杠杆
    pub max_leverage: u32,
    /// 是否已下架
    #[serde(default)]
    pub is_delisted: bool,
}

/// 资产上下文 (包含资金费率等实时数据)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// 当前资金费率
    pub funding: String,
    /// 持仓量
    pub open_interest: String,
    /// 标记价格
    pub mark_px: String,
    /// 中间价
    pub mid_px: String,
    /// oracle 价格
    pub oracle_px: String,
    /// 冲击价格 [bid_impact, ask_impact]
    pub impact_pxs: Option<Vec<String>>,
}

// ============================================================================
// WebSocket 消息结构
// ============================================================================

/// WebSocket 订阅响应
#[derive(Debug, Deserialize)]
pub struct WsSubscriptionResponse {
    pub channel: String,
    pub data: serde_json::Value,
}

/// AllMids 数据 (所有中间价)
#[derive(Debug, Deserialize)]
pub struct AllMids {
    pub mids: std::collections::HashMap<String, String>,
}

/// BBO 数据
#[derive(Debug, Deserialize)]
pub struct WsBbo {
    pub coin: String,
    pub time: u64,
    pub px: String,
    /// 买一价
    pub bid: String,
    /// 卖一价
    pub ask: String,
    /// 买一量
    #[serde(rename = "bidSz")]
    pub bid_sz: String,
    /// 卖一量
    #[serde(rename = "askSz")]
    pub ask_sz: String,
}

impl WsBbo {
    pub fn to_bbo(&self) -> BBO {
        let symbol = Symbol::from_hyperliquid(&self.coin);
        let bid_price = f64::from_str(&self.bid).unwrap_or(0.0);
        let bid_qty = f64::from_str(&self.bid_sz).unwrap_or(0.0);
        let ask_price = f64::from_str(&self.ask).unwrap_or(0.0);
        let ask_qty = f64::from_str(&self.ask_sz).unwrap_or(0.0);

        BBO {
            exchange: Exchange::Hyperliquid,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.time,
        }
    }
}

/// ActiveAssetCtx 数据 (实时资产上下文)
#[derive(Debug, Deserialize)]
pub struct WsActiveAssetCtx {
    pub coin: String,
    pub ctx: AssetCtx,
}

impl WsActiveAssetCtx {
    /// 转换为 FundingRate
    /// Hyperliquid 每小时结算一次资金费率
    pub fn to_funding_rate(&self) -> FundingRate {
        let symbol = Symbol::from_hyperliquid(&self.coin);
        let rate = f64::from_str(&self.ctx.funding).unwrap_or(0.0);

        FundingRate {
            exchange: Exchange::Hyperliquid,
            symbol,
            rate,
            // Hyperliquid 每小时整点结算，计算下一个整点时间
            next_settle_time: next_hourly_settle_time(),
            settle_interval_hours: 1.0,
        }
    }

    /// 转换为 BBO (从 impact_pxs 提取)
    pub fn to_bbo(&self) -> Option<BBO> {
        let impact_pxs = self.ctx.impact_pxs.as_ref()?;
        if impact_pxs.len() < 2 {
            return None;
        }

        let symbol = Symbol::from_hyperliquid(&self.coin);
        let bid_price = f64::from_str(&impact_pxs[0]).ok()?;
        let ask_price = f64::from_str(&impact_pxs[1]).ok()?;

        Some(BBO {
            exchange: Exchange::Hyperliquid,
            symbol,
            bid_price,
            bid_qty: 0.0, // impact price 不包含数量
            ask_price,
            ask_qty: 0.0,
            timestamp: now_ms(),
        })
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 计算下一个整点结算时间 (毫秒)
fn next_hourly_settle_time() -> u64 {
    let now = now_ms();
    let hour_ms = 3600 * 1000;
    let current_hour = now / hour_ms * hour_ms;
    current_hour + hour_ms
}

/// 计算价格精度 (Hyperliquid 使用 6 位小数)
pub fn price_step() -> f64 {
    0.000001 // 6 位小数
}

/// 计算数量精度
pub fn size_step(sz_decimals: i32) -> f64 {
    10f64.powi(-sz_decimals)
}

//! Hyperliquid 数据编解码
//!
//! 解析 Hyperliquid REST API 和 WebSocket 消息

#![allow(dead_code)]

use super::from_hyperliquid;
use crate::domain::{now_ms, Exchange, Fill, FundingRate, IndexPrice, MarkPrice, Side, BBO};
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
    /// 保证金模式 (None = 默认允许全仓, "strictIsolated" = 仅逐仓, "noCross" = 不支持全仓)
    pub margin_mode: Option<String>,
}

impl AssetInfo {
    /// 是否支持全仓保证金模式
    pub fn supports_cross_margin(&self) -> bool {
        match self.margin_mode.as_deref() {
            None => true, // 默认允许全仓
            Some("strictIsolated") | Some("noCross") => false,
            Some(_) => true, // 其他模式默认允许
        }
    }
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
/// API 格式: { coin, time, bbo: [bid_level | null, ask_level | null] }
#[derive(Debug, Deserialize)]
pub struct WsBbo {
    pub coin: String,
    pub time: u64,
    /// [bid, ask] - 每个可能为 null
    pub bbo: [Option<WsLevel>; 2],
}

/// BBO Level (价格层)
#[derive(Debug, Deserialize)]
pub struct WsLevel {
    /// 价格
    pub px: String,
    /// 数量
    pub sz: String,
    /// 订单数量
    pub n: u32,
}

impl WsBbo {
    pub fn to_bbo(&self) -> Result<BBO, String> {
        let symbol = from_hyperliquid(&self.coin);

        let bid = self.bbo[0].as_ref()
            .ok_or("BBO bid is null")?;
        let ask = self.bbo[1].as_ref()
            .ok_or("BBO ask is null")?;

        let bid_price = f64::from_str(&bid.px)
            .map_err(|_| format!("Failed to parse bid price: {}", bid.px))?;
        let bid_qty = f64::from_str(&bid.sz)
            .map_err(|_| format!("Failed to parse bid size: {}", bid.sz))?;
        let ask_price = f64::from_str(&ask.px)
            .map_err(|_| format!("Failed to parse ask price: {}", ask.px))?;
        let ask_qty = f64::from_str(&ask.sz)
            .map_err(|_| format!("Failed to parse ask size: {}", ask.sz))?;

        Ok(BBO {
            exchange: Exchange::Hyperliquid,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.time,
        })
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
    /// Hyperliquid 每小时整点结算
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, timestamp: u64) -> Result<FundingRate, String> {
        let symbol = from_hyperliquid(&self.coin);
        let rate = f64::from_str(&self.ctx.funding)
            .map_err(|_| format!("Failed to parse funding rate: {}", self.ctx.funding))?;

        Ok(FundingRate {
            exchange: Exchange::Hyperliquid,
            symbol,
            rate,
            // Hyperliquid 每小时整点结算，计算下一个整点时间
            next_settle_time: next_hourly_settle_time(),
            timestamp,
        })
    }

    /// 转换为 MarkPrice
    pub fn to_mark_price(&self, timestamp: u64) -> Result<MarkPrice, String> {
        let symbol = from_hyperliquid(&self.coin);
        let price = f64::from_str(&self.ctx.mark_px)
            .map_err(|_| format!("Failed to parse mark_px: {}", self.ctx.mark_px))?;

        Ok(MarkPrice {
            exchange: Exchange::Hyperliquid,
            symbol,
            price,
            timestamp,
        })
    }

    /// 转换为 IndexPrice (使用 oracle_px 作为指数价格)
    pub fn to_index_price(&self, timestamp: u64) -> Result<IndexPrice, String> {
        let symbol = from_hyperliquid(&self.coin);
        let price = f64::from_str(&self.ctx.oracle_px)
            .map_err(|_| format!("Failed to parse oracle_px: {}", self.ctx.oracle_px))?;

        Ok(IndexPrice {
            exchange: Exchange::Hyperliquid,
            symbol,
            price,
            timestamp,
        })
    }

    /// 获取 symbol
    pub fn symbol(&self) -> crate::domain::Symbol {
        from_hyperliquid(&self.coin)
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

/// 计算数量精度
pub fn size_step(sz_decimals: i32) -> f64 {
    10f64.powi(-sz_decimals)
}

// ============================================================================
// WebSocket 账户订阅消息结构
// ============================================================================

/// WebData3 响应 (账户状态)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsWebData3 {
    pub clearinghouse_state: Option<ClearinghouseState>,
}

/// Clearinghouse 状态 (仓位和余额)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    pub asset_positions: Vec<AssetPositionWrapper>,
    /// 总账户保证金摘要（全仓 + 逐仓）
    pub margin_summary: MarginSummary,
    /// 全仓部分的保证金摘要
    pub cross_margin_summary: MarginSummary,
    pub withdrawable: String,
}

/// MarginSummary (保证金摘要)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub account_value: String,
    pub total_ntl_pos: String,
    pub total_raw_usd: String,
    pub total_margin_used: String,
}

/// AssetPosition 包装器
#[derive(Debug, Deserialize)]
pub struct AssetPositionWrapper {
    pub position: AssetPosition,
    #[serde(rename = "type")]
    pub position_type: String,
}

/// AssetPosition (单个仓位)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPosition {
    pub coin: String,
    /// 带符号的仓位大小 (负数为空头)
    pub szi: String,
    pub entry_px: Option<String>,
    pub leverage: PositionLeverage,
    pub liquidation_px: Option<String>,
    pub unrealized_pnl: String,
    pub margin_used: String,
    pub position_value: String,
    pub return_on_equity: String,
    pub max_leverage: u32,
}

/// 杠杆信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionLeverage {
    #[serde(rename = "type")]
    pub leverage_type: String, // "cross" or "isolated"
    pub value: u32,
    pub raw_usd: Option<String>,
}

impl AssetPosition {
    pub fn to_position(&self) -> Result<crate::domain::Position, String> {
        let symbol = from_hyperliquid(&self.coin);
        let size = f64::from_str(&self.szi)
            .map_err(|_| format!("Failed to parse szi: {}", self.szi))?;
        // entry_px 在空仓 (size=0) 时可能为 None，此时使用 0.0
        let entry_price = match self.entry_px.as_deref() {
            Some(p) => f64::from_str(p).unwrap_or_else(|_| {
                tracing::warn!(coin = %self.coin, entry_px = %p, "Failed to parse entry_px, defaulting to 0.0");
                0.0
            }),
            None => 0.0,
        };
        let unrealized_pnl = f64::from_str(&self.unrealized_pnl)
            .map_err(|_| format!("Failed to parse unrealized_pnl: {}", self.unrealized_pnl))?;

        Ok(crate::domain::Position {
            exchange: Exchange::Hyperliquid,
            symbol,
            size,
            entry_price,
            unrealized_pnl,
        })
    }
}

/// OrderUpdates 响应
#[derive(Debug, Deserialize)]
pub struct WsOrderUpdate {
    pub order: WsBasicOrder,
    pub status: String,
    #[serde(rename = "statusTimestamp")]
    pub status_timestamp: u64,
}

/// 基本订单信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsBasicOrder {
    pub coin: String,
    pub side: String, // "A" (ask/sell) or "B" (bid/buy)
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    pub orig_sz: String,
    pub cloid: Option<String>,
}

impl WsOrderUpdate {
    pub fn to_order_update(&self) -> Result<crate::domain::OrderUpdate, String> {
        use crate::domain::Side;

        let symbol = from_hyperliquid(&self.order.coin);
        let orig_sz = f64::from_str(&self.order.orig_sz)
            .map_err(|_| format!("Failed to parse orig_sz: {}", self.order.orig_sz))?;
        let current_sz = f64::from_str(&self.order.sz)
            .map_err(|_| format!("Failed to parse sz: {}", self.order.sz))?;
        let filled_quantity = orig_sz - current_sz;

        // Hyperliquid: "A" = ask/sell, "B" = bid/buy
        let side = match self.order.side.as_str() {
            "B" => Side::Long,
            "A" => Side::Short,
            other => return Err(format!("Unknown Hyperliquid side: {}", other)),
        };

        let status = map_hyperliquid_order_status(&self.status, filled_quantity);

        // Hyperliquid 不直接提供本次成交量，使用 filled_quantity
        // 对于 IOC 订单（一次性成交），这等于实际成交量
        let price = f64::from_str(&self.order.limit_px)
            .map_err(|_| format!("Failed to parse limit_px: {}", self.order.limit_px))?;

        Ok(crate::domain::OrderUpdate {
            order_id: self.order.oid.to_string(),
            client_order_id: self.order.cloid.clone(),
            exchange: Exchange::Hyperliquid,
            symbol,
            side,
            status,
            price,
            quantity: orig_sz,
            filled_quantity,
            fill_sz: filled_quantity,
            timestamp: self.status_timestamp,
        })
    }
}

/// Hyperliquid 订单状态映射
fn map_hyperliquid_order_status(status: &str, filled: f64) -> crate::domain::OrderStatus {
    match status {
        "open" => {
            if filled > 0.0 {
                crate::domain::OrderStatus::PartiallyFilled { filled }
            } else {
                crate::domain::OrderStatus::Pending
            }
        }
        "filled" => crate::domain::OrderStatus::Filled,
        "canceled" | "cancelled" => crate::domain::OrderStatus::Cancelled,
        "rejected" => crate::domain::OrderStatus::Rejected {
            reason: "Order rejected".to_string(),
        },
        other => crate::domain::OrderStatus::Rejected {
            reason: format!("Unknown status: {}", other),
        },
    }
}

// ============================================================================
// userFills 消息结构
// ============================================================================

/// userFills 响应包装器
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsUserFills {
    pub user: String,
    pub fills: Vec<WsFill>,
    #[serde(default)]
    pub is_snapshot: bool,
}

/// 单个成交记录
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsFill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String, // "B" (buy) or "A" (sell/ask)
    pub time: u64,
    pub oid: u64,
    #[serde(default)]
    pub cloid: Option<String>,
    pub fee: String,
    pub closed_pnl: String,
}

impl WsFill {
    pub fn to_fill(&self) -> Result<Fill, String> {
        let symbol = from_hyperliquid(&self.coin);
        let price = f64::from_str(&self.px)
            .map_err(|_| format!("Failed to parse fill px: {}", self.px))?;
        let size = f64::from_str(&self.sz)
            .map_err(|_| format!("Failed to parse fill sz: {}", self.sz))?;

        // Hyperliquid: "B" = bid/buy, "A" = ask/sell
        let side = match self.side.as_str() {
            "B" => Side::Long,
            "A" => Side::Short,
            other => return Err(format!("Unknown Hyperliquid fill side: {}", other)),
        };

        let fee = f64::from_str(&self.fee)
            .map_err(|_| format!("Failed to parse fill fee: {}", self.fee))?;

        Ok(Fill {
            exchange: Exchange::Hyperliquid,
            symbol,
            side,
            price,
            size,
            client_order_id: self.cloid.clone(),
            order_id: self.oid.to_string(),
            timestamp: self.time,
            fee,
        })
    }
}

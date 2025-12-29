use crate::domain::{
    Exchange, FundingRate, OrderStatus, OrderUpdate, Position, Symbol, now_ms, BBO,
};
use serde::Deserialize;
use std::str::FromStr;

/// WebSocket 推送通用格式
#[derive(Debug, Deserialize)]
pub struct WsPush<T> {
    pub arg: WsArg,
    pub data: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsArg {
    #[allow(dead_code)]
    pub channel: String,
    pub inst_id: Option<String>,
    #[allow(dead_code)]
    pub inst_type: Option<String>,
}

/// Funding Rate 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingRateData {
    pub inst_id: String,
    #[allow(dead_code)]
    pub inst_type: String,
    pub funding_rate: String,
    #[allow(dead_code)]
    pub next_funding_rate: Option<String>,
    #[allow(dead_code)]
    pub funding_time: String,
    pub next_funding_time: String,
}

impl FundingRateData {
    pub fn to_funding_rate(&self) -> Option<FundingRate> {
        let symbol = Symbol::from_okx(&self.inst_id)?;
        let rate = f64::from_str(&self.funding_rate).ok()?;
        // funding_time: 下次收取时间
        // next_funding_time: 下下次收取时间 (用于计算间隔)
        let next_settle_ms: u64 = self.funding_time.parse().ok()?;
        let next_next_settle_ms: u64 = self.next_funding_time.parse().ok()?;

        // 计算结算间隔 (毫秒转小时)
        let interval_ms = next_next_settle_ms.saturating_sub(next_settle_ms);
        let settle_interval_hours = (interval_ms as f64) / (1000.0 * 60.0 * 60.0);

        Some(FundingRate {
            exchange: Exchange::OKX,
            symbol,
            rate,
            next_settle_time: next_settle_ms,
            settle_interval_hours,
        })
    }
}

/// BBO 数据 (bbo-tbt channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BboData {
    pub asks: Vec<Vec<String>>,
    pub bids: Vec<Vec<String>>,
    #[allow(dead_code)]
    pub ts: String,
    #[allow(dead_code)]
    pub seq_id: Option<i64>,
}

impl BboData {
    pub fn to_bbo(&self, inst_id: &str) -> Option<BBO> {
        let symbol = Symbol::from_okx(inst_id)?;

        let ask = self.asks.first()?;
        if ask.len() < 2 {
            return None;
        }
        let ask_price = f64::from_str(&ask[0]).ok()?;
        let ask_qty = f64::from_str(&ask[1]).ok()?;

        let bid = self.bids.first()?;
        if bid.len() < 2 {
            return None;
        }
        let bid_price = f64::from_str(&bid[0]).ok()?;
        let bid_qty = f64::from_str(&bid[1]).ok()?;

        Some(BBO {
            exchange: Exchange::OKX,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: now_ms(),
        })
    }
}

/// Position 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionData {
    pub inst_id: String,
    #[allow(dead_code)]
    pub inst_type: String,
    pub pos: String,
    #[allow(dead_code)]
    pub pos_side: String,
    pub avg_px: String,
    pub upl: String,
    pub lever: String,
    #[allow(dead_code)]
    pub mgn_mode: String,
    pub mark_px: Option<String>,
}

impl PositionData {
    pub fn to_position(&self) -> Option<Position> {
        let symbol = Symbol::from_okx(&self.inst_id)?;
        let pos_amount = f64::from_str(&self.pos).ok()?;
        let avg_price = f64::from_str(&self.avg_px).unwrap_or(0.0);
        let unrealized_pnl = f64::from_str(&self.upl).unwrap_or(0.0);
        let leverage: u32 = self.lever.parse().unwrap_or(1);
        let mark_price = self
            .mark_px
            .as_ref()
            .and_then(|p| f64::from_str(p).ok())
            .unwrap_or(0.0);

        Some(Position {
            exchange: Exchange::OKX,
            symbol,
            size: pos_amount, // 正数多头，负数空头
            entry_price: avg_price,
            leverage,
            unrealized_pnl,
            mark_price,
        })
    }
}

/// Account 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    #[allow(dead_code)]
    pub u_time: String,
    /// 账户总权益 (USDT)
    pub total_eq: String,
    #[allow(dead_code)]
    pub details: Vec<AccountDetail>,
}

impl AccountData {
    pub fn to_equity(&self) -> Option<f64> {
        f64::from_str(&self.total_eq).ok()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct AccountDetail {
    pub ccy: String,
    pub eq: String,
    pub avail_eq: String,
    pub avail_bal: String,
    pub frozen_bal: String,
}

/// Order 推送数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPushData {
    pub inst_id: String,
    pub ord_id: String,
    #[allow(dead_code)]
    pub cl_ord_id: Option<String>,
    pub state: String,
    #[allow(dead_code)]
    pub sz: String,
    pub fill_sz: String,
    pub avg_px: String,
    #[allow(dead_code)]
    pub fee: String,
    #[allow(dead_code)]
    pub fee_ccy: String,
}

impl OrderPushData {
    pub fn to_order_update(&self) -> Option<OrderUpdate> {
        let symbol = Symbol::from_okx(&self.inst_id)?;
        let filled_qty = f64::from_str(&self.fill_sz).ok()?;
        let avg_price = f64::from_str(&self.avg_px).ok();

        let status = map_okx_order_state(&self.state, filled_qty);

        Some(OrderUpdate {
            order_id: self.ord_id.clone(),
            client_order_id: self.cl_ord_id.clone(),
            exchange: Exchange::OKX,
            symbol,
            status,
            filled_quantity: filled_qty,
            avg_price,
            timestamp: now_ms(),
        })
    }
}

/// OKX 订单状态映射
fn map_okx_order_state(state: &str, filled: f64) -> OrderStatus {
    match state {
        "live" => OrderStatus::Pending,
        "partially_filled" => OrderStatus::PartiallyFilled { filled },
        "filled" => OrderStatus::Filled,
        "canceled" | "cancelled" => OrderStatus::Cancelled,
        other => OrderStatus::Rejected {
            reason: format!("Unknown state: {}", other),
        },
    }
}

/// WebSocket 事件响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsEvent {
    pub event: String,
    pub code: Option<String>,
    pub msg: Option<String>,
}

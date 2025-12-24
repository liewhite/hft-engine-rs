use crate::domain::{
    Balance, Exchange, FundingRate, OrderStatus, OrderUpdate, Position,
    Side, Symbol, now_ms, BBO,
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
    pub fn to_funding_rate(&self) -> FundingRate {
        let symbol = Symbol::from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Failed to parse OKX inst_id: {}", self.inst_id));
        let rate = f64::from_str(&self.funding_rate)
            .unwrap_or_else(|e| panic!("Failed to parse funding rate '{}': {}", self.funding_rate, e));
        let funding_time_ms: u64 = self.funding_time.parse()
            .unwrap_or_else(|e| panic!("Failed to parse funding_time '{}': {}", self.funding_time, e));
        let next_settle_ms: u64 = self.next_funding_time.parse()
            .unwrap_or_else(|e| panic!("Failed to parse next_funding_time '{}': {}", self.next_funding_time, e));

        // 计算结算间隔 (毫秒转小时)
        let interval_ms = next_settle_ms.saturating_sub(funding_time_ms);
        let settle_interval_hours = (interval_ms as f64) / (1000.0 * 60.0 * 60.0);

        FundingRate {
            exchange: Exchange::OKX,
            symbol,
            rate,
            next_settle_time: next_settle_ms,
            settle_interval_hours,
        }
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
    pub fn to_bbo(&self, inst_id: &str) -> BBO {
        let symbol = Symbol::from_okx(inst_id)
            .unwrap_or_else(|| panic!("Failed to parse OKX inst_id: {}", inst_id));

        let ask = self.asks.first()
            .unwrap_or_else(|| panic!("BBO missing asks for {}", inst_id));
        assert!(ask.len() >= 2, "BBO ask array too short for {}: {:?}", inst_id, ask);
        let ask_price = f64::from_str(&ask[0])
            .unwrap_or_else(|e| panic!("Failed to parse ask price '{}': {}", ask[0], e));
        let ask_qty = f64::from_str(&ask[1])
            .unwrap_or_else(|e| panic!("Failed to parse ask qty '{}': {}", ask[1], e));

        let bid = self.bids.first()
            .unwrap_or_else(|| panic!("BBO missing bids for {}", inst_id));
        assert!(bid.len() >= 2, "BBO bid array too short for {}: {:?}", inst_id, bid);
        let bid_price = f64::from_str(&bid[0])
            .unwrap_or_else(|e| panic!("Failed to parse bid price '{}': {}", bid[0], e));
        let bid_qty = f64::from_str(&bid[1])
            .unwrap_or_else(|e| panic!("Failed to parse bid qty '{}': {}", bid[1], e));

        BBO {
            exchange: Exchange::OKX,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: now_ms(),
        }
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
    pub fn to_position(&self) -> Position {
        let symbol = Symbol::from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Failed to parse OKX inst_id: {}", self.inst_id));
        let pos_amount = f64::from_str(&self.pos)
            .unwrap_or_else(|e| panic!("Failed to parse position '{}': {}", self.pos, e));
        let avg_price = f64::from_str(&self.avg_px).unwrap_or(0.0);
        let unrealized_pnl = f64::from_str(&self.upl).unwrap_or(0.0);
        let leverage: u32 = self.lever.parse().unwrap_or(1);
        let mark_price = self
            .mark_px
            .as_ref()
            .and_then(|p| f64::from_str(p).ok())
            .unwrap_or(0.0);

        let (side, size) = if pos_amount >= 0.0 {
            (Side::Long, pos_amount)
        } else {
            (Side::Short, pos_amount.abs())
        };

        Position {
            exchange: Exchange::OKX,
            symbol,
            side,
            size,
            entry_price: avg_price,
            leverage,
            unrealized_pnl,
            mark_price,
        }
    }
}

/// Account 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    #[allow(dead_code)]
    pub u_time: String,
    pub details: Vec<AccountDetail>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDetail {
    pub ccy: String,
    #[allow(dead_code)]
    pub eq: String,
    #[allow(dead_code)]
    pub avail_eq: String,
    pub avail_bal: String,
    pub frozen_bal: String,
}

impl AccountDetail {
    pub fn to_balance(&self) -> Balance {
        let available = f64::from_str(&self.avail_bal)
            .unwrap_or_else(|e| panic!("Failed to parse avail_bal '{}': {}", self.avail_bal, e));
        let frozen = f64::from_str(&self.frozen_bal)
            .unwrap_or_else(|e| panic!("Failed to parse frozen_bal '{}': {}", self.frozen_bal, e));

        Balance {
            exchange: Exchange::OKX,
            asset: self.ccy.clone(),
            available,
            frozen,
        }
    }
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
    pub fn to_order_update(&self) -> OrderUpdate {
        let symbol = Symbol::from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Failed to parse OKX inst_id: {}", self.inst_id));
        let filled_qty = f64::from_str(&self.fill_sz)
            .unwrap_or_else(|e| panic!("Failed to parse fill_sz '{}': {}", self.fill_sz, e));
        let avg_price = f64::from_str(&self.avg_px).ok();

        let status = map_okx_order_state(&self.state, filled_qty);

        OrderUpdate {
            order_id: self.ord_id.clone(),
            client_order_id: self.cl_ord_id.clone(),
            exchange: Exchange::OKX,
            symbol,
            status,
            filled_quantity: filled_qty,
            avg_price,
            timestamp: now_ms(),
        }
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

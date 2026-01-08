use super::{from_okx, from_okx_index};
use crate::domain::{Exchange, FundingRate, IndexPrice, MarkPrice, OrderStatus, OrderUpdate, Position, now_ms, BBO};
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
    // pub next_funding_time: String,
}

impl FundingRateData {
    /// 转换为 FundingRate
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, timestamp: u64) -> FundingRate {
        let symbol = from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", self.inst_id));
        let rate = f64::from_str(&self.funding_rate)
            .unwrap_or_else(|_| panic!("Failed to parse funding rate: {}", self.funding_rate));
        // funding_time: 下次收取时间
        let next_settle_ms: u64 = self.funding_time.parse()
            .unwrap_or_else(|_| panic!("Failed to parse funding time: {}", self.funding_time));

        FundingRate {
            exchange: Exchange::OKX,
            symbol,
            rate,
            next_settle_time: next_settle_ms,
            timestamp,
        }
    }
}

/// BBO 数据 (bbo-tbt channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BboData {
    pub asks: Vec<Vec<String>>,
    pub bids: Vec<Vec<String>>,
    pub ts: String,
    #[allow(dead_code)]
    pub seq_id: Option<i64>,
}

impl BboData {
    pub fn to_bbo(&self, inst_id: &str) -> BBO {
        let symbol = from_okx(inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", inst_id));

        let ask = self.asks.first()
            .expect("BBO asks is empty");
        assert!(ask.len() >= 2, "BBO ask data incomplete");
        let ask_price = f64::from_str(&ask[0])
            .unwrap_or_else(|_| panic!("Failed to parse ask price: {}", ask[0]));
        let ask_qty = f64::from_str(&ask[1])
            .unwrap_or_else(|_| panic!("Failed to parse ask qty: {}", ask[1]));

        let bid = self.bids.first()
            .expect("BBO bids is empty");
        assert!(bid.len() >= 2, "BBO bid data incomplete");
        let bid_price = f64::from_str(&bid[0])
            .unwrap_or_else(|_| panic!("Failed to parse bid price: {}", bid[0]));
        let bid_qty = f64::from_str(&bid[1])
            .unwrap_or_else(|_| panic!("Failed to parse bid qty: {}", bid[1]));

        let timestamp = self.ts.parse::<u64>()
            .unwrap_or_else(|_| panic!("Failed to parse timestamp: {}", self.ts));

        BBO {
            exchange: Exchange::OKX,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp,
        }
    }
}

/// Mark Price 数据 (mark-price channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkPriceData {
    pub inst_id: String,
    #[allow(dead_code)]
    pub inst_type: String,
    pub mark_px: String,
    pub ts: String,
}

impl MarkPriceData {
    pub fn to_mark_price(&self) -> MarkPrice {
        let symbol = from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", self.inst_id));
        let price = f64::from_str(&self.mark_px)
            .unwrap_or_else(|_| panic!("Failed to parse mark price: {}", self.mark_px));
        let timestamp = self.ts.parse::<u64>()
            .unwrap_or_else(|_| panic!("Failed to parse timestamp: {}", self.ts));

        MarkPrice {
            exchange: Exchange::OKX,
            symbol,
            price,
            timestamp,
        }
    }
}

/// Index Ticker 数据 (index-tickers channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexTickerData {
    pub inst_id: String,
    pub idx_px: String,
    pub ts: String,
}

impl IndexTickerData {
    pub fn to_index_price(&self) -> IndexPrice {
        // index-tickers 返回 BTC-USDT 格式，使用 from_okx_index 解析
        let symbol = from_okx_index(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX index symbol: {}", self.inst_id));
        let price = f64::from_str(&self.idx_px)
            .unwrap_or_else(|_| panic!("Failed to parse index price: {}", self.idx_px));
        let timestamp = self.ts.parse::<u64>()
            .unwrap_or_else(|_| panic!("Failed to parse timestamp: {}", self.ts));

        IndexPrice {
            exchange: Exchange::OKX,
            symbol,
            price,
            timestamp,
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
        let symbol = from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", self.inst_id));
        let pos_amount = f64::from_str(&self.pos)
            .unwrap_or_else(|_| panic!("Failed to parse position amount: {}", self.pos));
        // 以下字段可能为空字符串，使用默认值
        let avg_price = f64::from_str(&self.avg_px).unwrap_or(0.0);
        let unrealized_pnl = f64::from_str(&self.upl).unwrap_or(0.0);
        let leverage: u32 = self.lever.parse().unwrap_or(1);
        let mark_price = self
            .mark_px
            .as_ref()
            .and_then(|p| f64::from_str(p).ok())
            .unwrap_or(0.0);

        Position {
            exchange: Exchange::OKX,
            symbol,
            size: pos_amount, // 正数多头，负数空头
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
    /// 账户总权益 (USDT)
    pub total_eq: String,
    #[allow(dead_code)]
    pub details: Vec<AccountDetail>,
}

impl AccountData {
    pub fn to_equity(&self) -> f64 {
        f64::from_str(&self.total_eq)
            .unwrap_or_else(|_| panic!("Failed to parse total equity: {}", self.total_eq))
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
    pub fn to_order_update(&self) -> OrderUpdate {
        let symbol = from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", self.inst_id));
        let filled_qty = f64::from_str(&self.fill_sz)
            .unwrap_or_else(|_| panic!("Failed to parse filled qty: {}", self.fill_sz));
        // avg_price 可能为空字符串（未成交时）
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

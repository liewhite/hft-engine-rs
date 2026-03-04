use super::{from_okx, from_okx_index};
use crate::domain::{Candle, CandleInterval, Exchange, Fill, FundingRate, IndexPrice, MarkPrice, OrderStatus, OrderUpdate, Position, Side, now_ms, BBO};
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
    #[allow(dead_code)]
    pub lever: String,
    #[allow(dead_code)]
    pub mgn_mode: String,
}

impl PositionData {
    pub fn to_position(&self) -> Position {
        let symbol = from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", self.inst_id));
        let pos_amount = f64::from_str(&self.pos)
            .unwrap_or_else(|_| panic!("Failed to parse position amount: {}", self.pos));
        // 以下字段可能为空字符串，使用默认值
        let avg_price = f64::from_str(&self.avg_px).unwrap_or_else(|_| {
            if !self.avg_px.is_empty() {
                tracing::warn!(inst_id = %self.inst_id, avg_px = %self.avg_px, "Failed to parse OKX avg_px, defaulting to 0.0");
            }
            0.0
        });
        let unrealized_pnl = f64::from_str(&self.upl).unwrap_or_else(|_| {
            if !self.upl.is_empty() {
                tracing::warn!(inst_id = %self.inst_id, upl = %self.upl, "Failed to parse OKX upl, defaulting to 0.0");
            }
            0.0
        });

        Position {
            exchange: Exchange::OKX,
            symbol,
            size: pos_amount, // 正数多头，负数空头
            entry_price: avg_price,
            unrealized_pnl,
        }
    }
}

/// Account 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    pub u_time: String,
    /// 账户总权益 (USDT)
    pub total_eq: String,
    /// 账户总持仓名义价值 (USD)
    pub notional_usd: String,
    #[allow(dead_code)]
    pub details: Vec<AccountDetail>,
}

impl AccountData {
    pub fn to_equity(&self) -> f64 {
        f64::from_str(&self.total_eq)
            .expect("Failed to parse OKX total equity")
    }

    pub fn to_notional(&self) -> f64 {
        f64::from_str(&self.notional_usd)
            .expect("Failed to parse OKX notionalUsd")
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
    pub side: String, // "buy" or "sell"
    pub state: String,
    #[allow(dead_code)]
    pub sz: String,
    /// 本次成交数量
    pub fill_sz: String,
    /// 本次成交价格
    pub fill_px: String,
    /// 累计成交数量
    pub acc_fill_sz: String,
    #[allow(dead_code)]
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
        let fill_sz = f64::from_str(&self.fill_sz)
            .unwrap_or_else(|_| panic!("Failed to parse fill_sz: {}", self.fill_sz));
        let acc_fill_sz = f64::from_str(&self.acc_fill_sz)
            .unwrap_or_else(|_| panic!("Failed to parse acc_fill_sz: {}", self.acc_fill_sz));

        let side = match self.side.as_str() {
            "buy" => Side::Long,
            "sell" => Side::Short,
            other => panic!("Unknown OKX side: {}", other),
        };

        let status = map_okx_order_state(&self.state, acc_fill_sz);

        OrderUpdate {
            order_id: self.ord_id.clone(),
            client_order_id: self.cl_ord_id.clone(),
            exchange: Exchange::OKX,
            symbol,
            side,
            status,
            filled_quantity: acc_fill_sz,
            fill_sz,
            timestamp: now_ms(),
        }
    }

    /// 转换为 Fill 事件（仅当 fill_sz > 0 时有效）
    pub fn to_fill(&self) -> Option<Fill> {
        let fill_sz = f64::from_str(&self.fill_sz)
            .unwrap_or_else(|_| panic!("Failed to parse fill_sz: {}", self.fill_sz));

        // 没有成交则不生成 Fill
        if fill_sz == 0.0 {
            return None;
        }

        let symbol = from_okx(&self.inst_id)
            .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", self.inst_id));
        let fill_px = f64::from_str(&self.fill_px)
            .unwrap_or_else(|_| panic!("Failed to parse fill_px: {}", self.fill_px));

        let side = match self.side.as_str() {
            "buy" => Side::Long,
            "sell" => Side::Short,
            other => panic!("Unknown OKX side: {}", other),
        };

        Some(Fill {
            exchange: Exchange::OKX,
            symbol,
            side,
            price: fill_px,
            size: fill_sz,
            client_order_id: self.cl_ord_id.clone(),
            order_id: self.ord_id.clone(),
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

// ============================================================================
// Candle (K线)
// ============================================================================

/// OKX K线数据（字符串数组格式）
/// [ts, o, h, l, c, vol, volCcy, volCcyQuote, confirm]
pub type CandleRawData = Vec<String>;

/// 将 OKX K线原始数据转换为 Candle
pub fn parse_candle_data(
    raw: &CandleRawData,
    inst_id: &str,
    interval: CandleInterval,
) -> Candle {
    assert!(raw.len() >= 9, "OKX candle data incomplete: {:?}", raw);

    let symbol = from_okx(inst_id)
        .unwrap_or_else(|| panic!("Unknown OKX symbol: {}", inst_id));
    let open_time: u64 = raw[0].parse()
        .unwrap_or_else(|_| panic!("Failed to parse candle ts: {}", raw[0]));
    let open = f64::from_str(&raw[1])
        .unwrap_or_else(|_| panic!("Failed to parse candle open: {}", raw[1]));
    let high = f64::from_str(&raw[2])
        .unwrap_or_else(|_| panic!("Failed to parse candle high: {}", raw[2]));
    let low = f64::from_str(&raw[3])
        .unwrap_or_else(|_| panic!("Failed to parse candle low: {}", raw[3]));
    let close = f64::from_str(&raw[4])
        .unwrap_or_else(|_| panic!("Failed to parse candle close: {}", raw[4]));
    let volume = f64::from_str(&raw[5])
        .unwrap_or_else(|_| panic!("Failed to parse candle vol: {}", raw[5]));
    let confirm = &raw[8] == "1";

    Candle {
        exchange: Exchange::OKX,
        symbol,
        interval,
        open_time,
        open,
        high,
        low,
        close,
        volume,
        confirm,
        timestamp: open_time,
    }
}

/// CandleInterval → OKX bar 参数 (REST 和 WS channel 后缀)
pub fn candle_interval_to_okx_bar(interval: CandleInterval) -> &'static str {
    match interval {
        CandleInterval::Min1 => "1m",
        CandleInterval::Min3 => "3m",
        CandleInterval::Min5 => "5m",
        CandleInterval::Min15 => "15m",
        CandleInterval::Min30 => "30m",
        CandleInterval::Hour1 => "1H",
        CandleInterval::Hour2 => "2H",
        CandleInterval::Hour4 => "4H",
        CandleInterval::Hour6 => "6H",
        CandleInterval::Hour12 => "12H",
        CandleInterval::Day1 => "1D",
        CandleInterval::Week1 => "1W",
        CandleInterval::Month1 => "1M",
        CandleInterval::Month3 => "3M",
    }
}

/// OKX WS channel → CandleInterval
pub fn okx_channel_to_candle_interval(channel: &str) -> Option<CandleInterval> {
    match channel {
        "candle1m" => Some(CandleInterval::Min1),
        "candle3m" => Some(CandleInterval::Min3),
        "candle5m" => Some(CandleInterval::Min5),
        "candle15m" => Some(CandleInterval::Min15),
        "candle30m" => Some(CandleInterval::Min30),
        "candle1H" => Some(CandleInterval::Hour1),
        "candle2H" => Some(CandleInterval::Hour2),
        "candle4H" => Some(CandleInterval::Hour4),
        "candle6H" => Some(CandleInterval::Hour6),
        "candle12H" => Some(CandleInterval::Hour12),
        "candle1D" => Some(CandleInterval::Day1),
        "candle1W" => Some(CandleInterval::Week1),
        "candle1M" => Some(CandleInterval::Month1),
        "candle3M" => Some(CandleInterval::Month3),
        _ => None,
    }
}

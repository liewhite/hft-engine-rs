use super::{from_okx, from_okx_index};
use crate::domain::{Candle, CandleInterval, Exchange, Fill, FundingRate, Greeks, IndexPrice, MarkPrice, OrderStatus, OrderUpdate, Position, Side, now_ms, BBO};
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
    pub fn to_funding_rate(&self, timestamp: u64) -> Result<FundingRate, String> {
        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        let rate = f64::from_str(&self.funding_rate)
            .map_err(|_| format!("Failed to parse funding rate: {}", self.funding_rate))?;
        // funding_time: 下次收取时间
        let next_settle_ms: u64 = self.funding_time.parse()
            .map_err(|_| format!("Failed to parse funding time: {}", self.funding_time))?;

        Ok(FundingRate {
            exchange: Exchange::OKX,
            symbol,
            rate,
            next_settle_time: next_settle_ms,
            timestamp,
        })
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
    pub fn to_bbo(&self, inst_id: &str) -> Result<BBO, String> {
        let symbol = from_okx(inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", inst_id))?;

        let ask = self.asks.first()
            .ok_or("BBO asks is empty")?;
        if ask.len() < 2 {
            return Err(format!("BBO ask data incomplete: {:?}", ask));
        }
        let ask_price = f64::from_str(&ask[0])
            .map_err(|_| format!("Failed to parse ask price: {}", ask[0]))?;
        let ask_qty = f64::from_str(&ask[1])
            .map_err(|_| format!("Failed to parse ask qty: {}", ask[1]))?;

        let bid = self.bids.first()
            .ok_or("BBO bids is empty")?;
        if bid.len() < 2 {
            return Err(format!("BBO bid data incomplete: {:?}", bid));
        }
        let bid_price = f64::from_str(&bid[0])
            .map_err(|_| format!("Failed to parse bid price: {}", bid[0]))?;
        let bid_qty = f64::from_str(&bid[1])
            .map_err(|_| format!("Failed to parse bid qty: {}", bid[1]))?;

        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(BBO {
            exchange: Exchange::OKX,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp,
        })
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
    pub fn to_mark_price(&self) -> Result<MarkPrice, String> {
        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        let price = f64::from_str(&self.mark_px)
            .map_err(|_| format!("Failed to parse mark price: {}", self.mark_px))?;
        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(MarkPrice {
            exchange: Exchange::OKX,
            symbol,
            price,
            timestamp,
        })
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
    pub fn to_index_price(&self) -> Result<IndexPrice, String> {
        // index-tickers 返回 BTC-USDT 格式，使用 from_okx_index 解析
        let symbol = from_okx_index(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX index symbol: {}", self.inst_id))?;
        let price = f64::from_str(&self.idx_px)
            .map_err(|_| format!("Failed to parse index price: {}", self.idx_px))?;
        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(IndexPrice {
            exchange: Exchange::OKX,
            symbol,
            price,
            timestamp,
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
    #[allow(dead_code)]
    pub lever: String,
    #[allow(dead_code)]
    pub mgn_mode: String,
}

impl PositionData {
    pub fn to_position(&self) -> Result<Position, String> {
        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        let pos_amount = f64::from_str(&self.pos)
            .map_err(|_| format!("Failed to parse position amount: {}", self.pos))?;
        // OKX 空仓时 avg_px/upl 为空字符串，此时为 0.0；非空必须解析成功
        let avg_price = if self.avg_px.is_empty() {
            0.0
        } else {
            f64::from_str(&self.avg_px)
                .map_err(|_| format!("Failed to parse avg_px: {}", self.avg_px))?
        };
        let unrealized_pnl = if self.upl.is_empty() {
            0.0
        } else {
            f64::from_str(&self.upl)
                .map_err(|_| format!("Failed to parse upl: {}", self.upl))?
        };

        Ok(Position {
            exchange: Exchange::OKX,
            symbol,
            size: pos_amount, // 正数多头，负数空头
            entry_price: avg_price,
            unrealized_pnl,
        })
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
    pub details: Vec<AccountDetail>,
}

impl AccountData {
    pub fn to_equity(&self) -> Result<f64, String> {
        f64::from_str(&self.total_eq)
            .map_err(|_| format!("Failed to parse OKX total equity: {}", self.total_eq))
    }

    pub fn to_notional(&self) -> Result<f64, String> {
        f64::from_str(&self.notional_usd)
            .map_err(|_| format!("Failed to parse OKX notionalUsd: {}", self.notional_usd))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDetail {
    pub ccy: String,
    #[allow(dead_code)]
    pub eq: String,
    #[allow(dead_code)]
    pub avail_eq: String,
    #[allow(dead_code)]
    pub avail_bal: String,
    #[allow(dead_code)]
    pub frozen_bal: String,
    /// 币种现金余额 (现货持有量)
    pub cash_bal: String,
}

/// Order 推送数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPushData {
    pub inst_id: String,
    pub ord_id: String,
    pub cl_ord_id: Option<String>,
    pub side: String, // "buy" or "sell"
    pub state: String,
    /// 订单价格 (限价单)
    pub px: String,
    /// 订单总数量 (张)
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
    pub fn to_order_update(&self) -> Result<OrderUpdate, String> {
        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        // market order px 为空字符串，此时 price 为 0.0
        let price = if self.px.is_empty() {
            0.0
        } else {
            f64::from_str(&self.px)
                .map_err(|_| format!("Failed to parse px: {}", self.px))?
        };
        let sz = f64::from_str(&self.sz)
            .map_err(|_| format!("Failed to parse sz: {}", self.sz))?;
        let fill_sz = f64::from_str(&self.fill_sz)
            .map_err(|_| format!("Failed to parse fill_sz: {}", self.fill_sz))?;
        let acc_fill_sz = f64::from_str(&self.acc_fill_sz)
            .map_err(|_| format!("Failed to parse acc_fill_sz: {}", self.acc_fill_sz))?;

        let side = match self.side.as_str() {
            "buy" => Side::Long,
            "sell" => Side::Short,
            other => return Err(format!("Unknown OKX side: {}", other)),
        };

        let status = map_okx_order_state(&self.state, acc_fill_sz);

        Ok(OrderUpdate {
            order_id: self.ord_id.clone(),
            client_order_id: self.cl_ord_id.clone(),
            exchange: Exchange::OKX,
            symbol,
            side,
            status,
            price,
            quantity: sz,          // 张数，由 private_ws 转换为币
            filled_quantity: acc_fill_sz, // 张数，由 private_ws 转换为币
            fill_sz,               // 张数，由 private_ws 转换为币
            timestamp: now_ms(),
        })
    }

    /// 转换为 Fill 事件（仅当 fill_sz > 0 时有效）
    pub fn to_fill(&self) -> Result<Option<Fill>, String> {
        let fill_sz = f64::from_str(&self.fill_sz)
            .map_err(|_| format!("Failed to parse fill_sz: {}", self.fill_sz))?;

        // 没有成交则不生成 Fill
        if fill_sz == 0.0 {
            return Ok(None);
        }

        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        let fill_px = f64::from_str(&self.fill_px)
            .map_err(|_| format!("Failed to parse fill_px: {}", self.fill_px))?;

        let side = match self.side.as_str() {
            "buy" => Side::Long,
            "sell" => Side::Short,
            other => return Err(format!("Unknown OKX side: {}", other)),
        };

        // OKX: fee 为负数表示收费，取反统一为正数=收费
        let fee = f64::from_str(&self.fee)
            .map_err(|_| format!("Failed to parse fee: {}", self.fee))?;

        Ok(Some(Fill {
            exchange: Exchange::OKX,
            symbol,
            side,
            price: fill_px,
            size: fill_sz,
            client_order_id: self.cl_ord_id.clone(),
            order_id: self.ord_id.clone(),
            timestamp: now_ms(),
            fee: -fee,
        }))
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

/// Account Greeks 数据 (account-greeks channel)
#[derive(Debug, Deserialize)]
pub struct GreeksData {
    pub ccy: String,
    #[serde(rename = "deltaBS")]
    pub delta_bs: String,
    #[serde(rename = "gammaBS")]
    pub gamma_bs: String,
    #[serde(rename = "thetaBS")]
    pub theta_bs: String,
    #[serde(rename = "vegaBS")]
    pub vega_bs: String,
    pub ts: String,
}

impl GreeksData {
    pub fn to_greeks(&self) -> Result<Greeks, String> {
        let delta = f64::from_str(&self.delta_bs)
            .map_err(|_| format!("Failed to parse deltaBS: {}", self.delta_bs))?;
        let gamma = f64::from_str(&self.gamma_bs)
            .map_err(|_| format!("Failed to parse gammaBS: {}", self.gamma_bs))?;
        let theta = f64::from_str(&self.theta_bs)
            .map_err(|_| format!("Failed to parse thetaBS: {}", self.theta_bs))?;
        let vega = f64::from_str(&self.vega_bs)
            .map_err(|_| format!("Failed to parse vegaBS: {}", self.vega_bs))?;
        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(Greeks {
            exchange: Exchange::OKX,
            ccy: self.ccy.clone(),
            delta,
            gamma,
            theta,
            vega,
            timestamp,
        })
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
) -> Result<Candle, String> {
    if raw.len() < 9 {
        return Err(format!("OKX candle data incomplete: {:?}", raw));
    }

    let symbol = from_okx(inst_id)
        .ok_or_else(|| format!("Unknown OKX symbol: {}", inst_id))?;
    let open_time: u64 = raw[0].parse()
        .map_err(|_| format!("Failed to parse candle ts: {}", raw[0]))?;
    let open = f64::from_str(&raw[1])
        .map_err(|_| format!("Failed to parse candle open: {}", raw[1]))?;
    let high = f64::from_str(&raw[2])
        .map_err(|_| format!("Failed to parse candle high: {}", raw[2]))?;
    let low = f64::from_str(&raw[3])
        .map_err(|_| format!("Failed to parse candle low: {}", raw[3]))?;
    let close = f64::from_str(&raw[4])
        .map_err(|_| format!("Failed to parse candle close: {}", raw[4]))?;
    let volume = f64::from_str(&raw[5])
        .map_err(|_| format!("Failed to parse candle vol: {}", raw[5]))?;
    let confirm = &raw[8] == "1";

    Ok(Candle {
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
    })
}

/// CandleInterval → OKX bar 参数 (REST 和 WS channel 后缀)
///
/// OKX 格式恰好与 CandleInterval::Display 一致
pub fn candle_interval_to_okx_bar(interval: CandleInterval) -> String {
    interval.to_string()
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

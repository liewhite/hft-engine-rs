use super::from_binance;
use crate::domain::{
    Balance, Exchange, Fill, FundingRate, IndexPrice, MarkPrice, OrderStatus, OrderUpdate, Position, Side, now_ms, BBO,
};
use serde::Deserialize;
use std::str::FromStr;

/// Mark Price 更新 (包含资金费率)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MarkPriceUpdate {
    pub e: String,
    pub s: String,
    pub p: String,
    pub i: String,
    pub r: String,
    #[serde(rename = "T")]
    pub t: i64,
}

impl MarkPriceUpdate {
    /// 转换为 FundingRate
    /// quote: 计价币种（用于解析 symbol）
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, quote: &str, timestamp: u64) -> Result<FundingRate, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let rate = f64::from_str(&self.r)
            .map_err(|_| format!("Failed to parse funding rate: {}", self.r))?;

        Ok(FundingRate {
            exchange: Exchange::Binance,
            symbol,
            rate,
            next_settle_time: self.t as u64,
            timestamp,
        })
    }

    /// 转换为 MarkPrice
    pub fn to_mark_price(&self, quote: &str, timestamp: u64) -> Result<MarkPrice, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let price = f64::from_str(&self.p)
            .map_err(|_| format!("Failed to parse mark price: {}", self.p))?;

        Ok(MarkPrice {
            exchange: Exchange::Binance,
            symbol,
            price,
            timestamp,
        })
    }

    /// 转换为 IndexPrice
    pub fn to_index_price(&self, quote: &str, timestamp: u64) -> Result<IndexPrice, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let price = f64::from_str(&self.i)
            .map_err(|_| format!("Failed to parse index price: {}", self.i))?;

        Ok(IndexPrice {
            exchange: Exchange::Binance,
            symbol,
            price,
            timestamp,
        })
    }

    /// 获取 symbol (用于查询结算间隔)
    pub fn symbol(&self, quote: &str) -> Result<crate::domain::Symbol, String> {
        from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))
    }
}

/// Book Ticker (BBO)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BookTicker {
    pub e: String,
    pub s: String,
    pub b: String,
    #[serde(rename = "B")]
    pub bid_qty: String,
    pub a: String,
    #[serde(rename = "A")]
    pub ask_qty: String,
    #[serde(rename = "T")]
    pub t: i64,
}

impl BookTicker {
    pub fn to_bbo(&self, quote: &str) -> Result<BBO, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let bid_price = f64::from_str(&self.b)
            .map_err(|_| format!("Failed to parse bid price: {}", self.b))?;
        let bid_qty = f64::from_str(&self.bid_qty)
            .map_err(|_| format!("Failed to parse bid qty: {}", self.bid_qty))?;
        let ask_price = f64::from_str(&self.a)
            .map_err(|_| format!("Failed to parse ask price: {}", self.a))?;
        let ask_qty = f64::from_str(&self.ask_qty)
            .map_err(|_| format!("Failed to parse ask qty: {}", self.ask_qty))?;

        Ok(BBO {
            exchange: Exchange::Binance,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.t as u64,
        })
    }
}

/// 账户更新
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccountUpdate {
    pub e: String,
    pub a: AccountData,
}

#[derive(Debug, Deserialize)]
pub struct AccountData {
    #[serde(rename = "B")]
    pub balances: Vec<AccountBalance>,
    #[serde(rename = "P")]
    pub positions: Vec<AccountPosition>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccountBalance {
    pub a: String,
    pub wb: String,
    pub cw: String,
    pub bc: String,
}

impl AccountBalance {
    pub fn to_balance(&self) -> Result<Balance, String> {
        let available = f64::from_str(&self.cw)
            .map_err(|_| format!("Failed to parse available balance: {}", self.cw))?;
        let wallet_balance = f64::from_str(&self.wb)
            .map_err(|_| format!("Failed to parse wallet balance: {}", self.wb))?;
        let frozen = (wallet_balance - available).max(0.0);

        Ok(Balance {
            exchange: Exchange::Binance,
            asset: self.a.clone(),
            available,
            frozen,
        })
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccountPosition {
    pub s: String,
    pub pa: String,
    pub ep: String,
    pub up: String,
    pub mt: String,
    pub ps: String,
}

impl AccountPosition {
    pub fn to_position(&self, quote: &str) -> Result<Position, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let pos_amount = f64::from_str(&self.pa)
            .map_err(|_| format!("Failed to parse position amount: {}", self.pa))?;
        let entry_price = f64::from_str(&self.ep)
            .map_err(|_| format!("Failed to parse entry price: {}", self.ep))?;
        let unrealized_pnl = f64::from_str(&self.up)
            .map_err(|_| format!("Failed to parse unrealized pnl: {}", self.up))?;

        Ok(Position {
            exchange: Exchange::Binance,
            symbol,
            size: pos_amount, // 正数多头，负数空头
            entry_price,
            unrealized_pnl,
        })
    }
}

/// 订单更新
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OrderTradeUpdate {
    pub e: String,
    pub o: OrderData,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OrderData {
    pub s: String,
    pub c: String,  // client_order_id
    #[serde(rename = "S")]
    pub side: String, // "BUY" or "SELL"
    pub i: i64,
    #[serde(rename = "X")]
    pub status: String,
    pub q: String,
    /// 累计成交量 (Cumulative filled quantity)
    pub z: String,
    /// 本次成交量 (Last filled quantity)
    pub l: String,
    /// 本次成交价格 (Last filled price)
    #[serde(rename = "L")]
    pub last_price: String,
    pub ap: String,
    pub rp: String,
}

impl OrderTradeUpdate {
    pub fn to_order_update(&self, quote: &str) -> Result<OrderUpdate, String> {
        let symbol = from_binance(&self.o.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.o.s))?;
        let filled_qty = f64::from_str(&self.o.z)
            .map_err(|_| format!("Failed to parse filled qty: {}", self.o.z))?;
        let fill_sz = f64::from_str(&self.o.l)
            .map_err(|_| format!("Failed to parse last filled qty: {}", self.o.l))?;

        let side = match self.o.side.as_str() {
            "BUY" => Side::Long,
            "SELL" => Side::Short,
            other => return Err(format!("Unknown Binance side: {}", other)),
        };

        let status = match self.o.status.as_str() {
            "NEW" => OrderStatus::Pending,
            "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled { filled: filled_qty },
            "FILLED" => OrderStatus::Filled,
            "CANCELED" | "CANCELLED" => OrderStatus::Cancelled,
            "REJECTED" | "EXPIRED" => OrderStatus::Rejected {
                reason: self.o.status.clone(),
            },
            other => OrderStatus::Rejected {
                reason: format!("Unknown status: {}", other),
            },
        };

        let price = f64::from_str(&self.o.ap).unwrap_or(0.0);
        let quantity = f64::from_str(&self.o.q).unwrap_or(0.0);

        Ok(OrderUpdate {
            order_id: self.o.i.to_string(),
            client_order_id: if self.o.c.is_empty() { None } else { Some(self.o.c.clone()) },
            exchange: Exchange::Binance,
            symbol,
            side,
            status,
            price,
            quantity,
            filled_quantity: filled_qty,
            fill_sz,
            timestamp: now_ms(),
        })
    }

    /// 转换为 Fill 事件（仅当 fill_sz > 0 时有效）
    pub fn to_fill(&self, quote: &str) -> Result<Option<Fill>, String> {
        let fill_sz = f64::from_str(&self.o.l)
            .map_err(|_| format!("Failed to parse last filled qty: {}", self.o.l))?;

        // 没有成交则不生成 Fill
        if fill_sz == 0.0 {
            return Ok(None);
        }

        let symbol = from_binance(&self.o.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.o.s))?;
        let last_price = f64::from_str(&self.o.last_price)
            .map_err(|_| format!("Failed to parse last price: {}", self.o.last_price))?;

        let side = match self.o.side.as_str() {
            "BUY" => Side::Long,
            "SELL" => Side::Short,
            other => return Err(format!("Unknown Binance side: {}", other)),
        };

        Ok(Some(Fill {
            exchange: Exchange::Binance,
            symbol,
            side,
            price: last_price,
            size: fill_sz,
            client_order_id: if self.o.c.is_empty() { None } else { Some(self.o.c.clone()) },
            order_id: self.o.i.to_string(),
            timestamp: now_ms(),
        }))
    }
}

/// WebSocket 订阅响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsResponse {
    pub result: Option<serde_json::Value>,
    pub error: Option<WsError>,
}

/// WebSocket 错误
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsError {
    pub code: i32,
    pub msg: String,
}

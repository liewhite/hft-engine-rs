use super::from_binance;
use crate::domain::{
    Balance, Exchange, FundingRate, IndexPrice, MarkPrice, OrderStatus, OrderUpdate, Position, now_ms, BBO,
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
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, timestamp: u64) -> FundingRate {
        let symbol = from_binance(&self.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.s));
        let rate = f64::from_str(&self.r)
            .unwrap_or_else(|_| panic!("Failed to parse funding rate: {}", self.r));

        FundingRate {
            exchange: Exchange::Binance,
            symbol,
            rate,
            next_settle_time: self.t as u64,
            timestamp,
        }
    }

    /// 转换为 MarkPrice
    pub fn to_mark_price(&self, timestamp: u64) -> MarkPrice {
        let symbol = from_binance(&self.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.s));
        let price = f64::from_str(&self.p)
            .unwrap_or_else(|_| panic!("Failed to parse mark price: {}", self.p));

        MarkPrice {
            exchange: Exchange::Binance,
            symbol,
            price,
            timestamp,
        }
    }

    /// 转换为 IndexPrice
    pub fn to_index_price(&self, timestamp: u64) -> IndexPrice {
        let symbol = from_binance(&self.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.s));
        let price = f64::from_str(&self.i)
            .unwrap_or_else(|_| panic!("Failed to parse index price: {}", self.i));

        IndexPrice {
            exchange: Exchange::Binance,
            symbol,
            price,
            timestamp,
        }
    }

    /// 获取 symbol (用于查询结算间隔)
    pub fn symbol(&self) -> crate::domain::Symbol {
        from_binance(&self.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.s))
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
    pub fn to_bbo(&self) -> BBO {
        let symbol = from_binance(&self.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.s));
        let bid_price = f64::from_str(&self.b)
            .unwrap_or_else(|_| panic!("Failed to parse bid price: {}", self.b));
        let bid_qty = f64::from_str(&self.bid_qty)
            .unwrap_or_else(|_| panic!("Failed to parse bid qty: {}", self.bid_qty));
        let ask_price = f64::from_str(&self.a)
            .unwrap_or_else(|_| panic!("Failed to parse ask price: {}", self.a));
        let ask_qty = f64::from_str(&self.ask_qty)
            .unwrap_or_else(|_| panic!("Failed to parse ask qty: {}", self.ask_qty));

        BBO {
            exchange: Exchange::Binance,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.t as u64,
        }
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
    pub fn to_balance(&self) -> Balance {
        let available = f64::from_str(&self.cw)
            .unwrap_or_else(|_| panic!("Failed to parse available balance: {}", self.cw));
        let wallet_balance = f64::from_str(&self.wb)
            .unwrap_or_else(|_| panic!("Failed to parse wallet balance: {}", self.wb));
        let frozen = (wallet_balance - available).max(0.0);

        Balance {
            exchange: Exchange::Binance,
            asset: self.a.clone(),
            available,
            frozen,
        }
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
    pub fn to_position(&self) -> Position {
        let symbol = from_binance(&self.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.s));
        let pos_amount = f64::from_str(&self.pa)
            .unwrap_or_else(|_| panic!("Failed to parse position amount: {}", self.pa));
        let entry_price = f64::from_str(&self.ep)
            .unwrap_or_else(|_| panic!("Failed to parse entry price: {}", self.ep));
        let unrealized_pnl = f64::from_str(&self.up)
            .unwrap_or_else(|_| panic!("Failed to parse unrealized pnl: {}", self.up));

        Position {
            exchange: Exchange::Binance,
            symbol,
            size: pos_amount, // 正数多头，负数空头
            entry_price,
            leverage: 1, // 需要从其他接口获取
            unrealized_pnl,
            mark_price: 0.0, // 需要从 mark price 更新
        }
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
    pub i: i64,
    #[serde(rename = "X")]
    pub status: String,
    pub q: String,
    pub z: String,
    pub ap: String,
    pub rp: String,
}

impl OrderTradeUpdate {
    pub fn to_order_update(&self) -> OrderUpdate {
        let symbol = from_binance(&self.o.s)
            .unwrap_or_else(|| panic!("Unknown Binance symbol: {}", self.o.s));
        let filled_qty = f64::from_str(&self.o.z)
            .unwrap_or_else(|_| panic!("Failed to parse filled qty: {}", self.o.z));
        // avg_price 可能为空字符串（未成交时）
        let avg_price = f64::from_str(&self.o.ap).ok();

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

        OrderUpdate {
            order_id: self.o.i.to_string(),
            client_order_id: if self.o.c.is_empty() { None } else { Some(self.o.c.clone()) },
            exchange: Exchange::Binance,
            symbol,
            status,
            filled_quantity: filled_qty,
            avg_price,
            timestamp: now_ms(),
        }
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

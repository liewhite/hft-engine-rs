use crate::domain::{
    Balance, Exchange, FundingRate, OrderStatus, OrderUpdate, Position, Symbol, now_ms, BBO,
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
    /// settle_interval_hours: 结算间隔小时数 (从 fundingInfo API 获取，默认 8)
    pub fn to_funding_rate(&self, settle_interval_hours: f64) -> Option<FundingRate> {
        let symbol = Symbol::from_binance(&self.s)?;
        let rate = f64::from_str(&self.r).ok()?;

        Some(FundingRate {
            exchange: Exchange::Binance,
            symbol,
            rate,
            next_settle_time: self.t as u64,
            settle_interval_hours,
        })
    }

    /// 获取 symbol (用于查询结算间隔)
    pub fn symbol(&self) -> Option<Symbol> {
        Symbol::from_binance(&self.s)
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
    pub fn to_bbo(&self) -> Option<BBO> {
        let symbol = Symbol::from_binance(&self.s)?;
        let bid_price = f64::from_str(&self.b).ok()?;
        let bid_qty = f64::from_str(&self.bid_qty).ok()?;
        let ask_price = f64::from_str(&self.a).ok()?;
        let ask_qty = f64::from_str(&self.ask_qty).ok()?;

        Some(BBO {
            exchange: Exchange::Binance,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: now_ms(),
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
    pub fn to_balance(&self) -> Option<Balance> {
        let available = f64::from_str(&self.cw).ok()?;
        let wallet_balance = f64::from_str(&self.wb).ok()?;
        let frozen = (wallet_balance - available).max(0.0);

        Some(Balance {
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
    pub fn to_position(&self) -> Option<Position> {
        let symbol = Symbol::from_binance(&self.s)?;
        let pos_amount = f64::from_str(&self.pa).ok()?;
        let entry_price = f64::from_str(&self.ep).ok()?;
        let unrealized_pnl = f64::from_str(&self.up).ok()?;

        Some(Position {
            exchange: Exchange::Binance,
            symbol,
            size: pos_amount, // 正数多头，负数空头
            entry_price,
            leverage: 1, // 需要从其他接口获取
            unrealized_pnl,
            mark_price: 0.0, // 需要从 mark price 更新
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
    pub i: i64,
    #[serde(rename = "X")]
    pub status: String,
    pub q: String,
    pub z: String,
    pub ap: String,
    pub rp: String,
}

impl OrderTradeUpdate {
    pub fn to_order_update(&self) -> Option<OrderUpdate> {
        let symbol = Symbol::from_binance(&self.o.s)?;
        let filled_qty = f64::from_str(&self.o.z).ok()?;
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

        Some(OrderUpdate {
            order_id: self.o.i.to_string(),
            client_order_id: if self.o.c.is_empty() { None } else { Some(self.o.c.clone()) },
            exchange: Exchange::Binance,
            symbol,
            status,
            filled_quantity: filled_qty,
            avg_price,
            timestamp: now_ms(),
        })
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

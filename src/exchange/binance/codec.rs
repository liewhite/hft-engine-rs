use crate::domain::{
    Balance, Exchange, FundingRate, OrderId, OrderStatus, OrderUpdate, Position, Price, Quantity,
    Rate, Side, Symbol, BBO,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use std::time::{Duration, Instant};

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
    pub fn to_funding_rate(&self) -> Option<FundingRate> {
        let symbol = Symbol::from_binance(&self.s)?;
        let rate = Decimal::from_str(&self.r).ok()?;
        let next_settle_ms = self.t as u64;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let settle_in = if next_settle_ms > now_ms {
            Duration::from_millis(next_settle_ms - now_ms)
        } else {
            Duration::ZERO
        };

        Some(FundingRate {
            exchange: Exchange::Binance,
            symbol,
            rate: Rate(rate),
            next_settle_time: Instant::now() + settle_in,
        })
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
        let bid_price = Decimal::from_str(&self.b).ok()?;
        let bid_qty = Decimal::from_str(&self.bid_qty).ok()?;
        let ask_price = Decimal::from_str(&self.a).ok()?;
        let ask_qty = Decimal::from_str(&self.ask_qty).ok()?;

        Some(BBO {
            exchange: Exchange::Binance,
            symbol,
            bid_price: Price(bid_price),
            bid_qty: Quantity(bid_qty),
            ask_price: Price(ask_price),
            ask_qty: Quantity(ask_qty),
            timestamp: Instant::now(),
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
        let available = Decimal::from_str(&self.cw).ok()?;
        let frozen = Decimal::from_str(&self.wb).ok()? - available;

        Some(Balance {
            exchange: Exchange::Binance,
            asset: self.a.clone(),
            available,
            frozen: frozen.max(Decimal::ZERO),
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
        let pos_amount = Decimal::from_str(&self.pa).ok()?;
        let entry_price = Decimal::from_str(&self.ep).ok()?;
        let unrealized_pnl = Decimal::from_str(&self.up).ok()?;

        let (side, size) = if pos_amount >= Decimal::ZERO {
            (Side::Long, Quantity(pos_amount))
        } else {
            (Side::Short, Quantity(pos_amount.abs()))
        };

        Some(Position {
            exchange: Exchange::Binance,
            symbol,
            side,
            size,
            entry_price: Price(entry_price),
            leverage: 1, // 需要从其他接口获取
            unrealized_pnl,
            mark_price: Price::ZERO, // 需要从 mark price 更新
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
        let filled_qty = Decimal::from_str(&self.o.z).ok()?;
        let avg_price = Decimal::from_str(&self.o.ap).ok();

        let status = match self.o.status.as_str() {
            "NEW" => OrderStatus::Pending,
            "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled {
                filled: Quantity(filled_qty),
            },
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
            order_id: OrderId::from(self.o.i),
            exchange: Exchange::Binance,
            symbol,
            status,
            filled_quantity: Quantity(filled_qty),
            avg_price: avg_price.map(Price),
            timestamp: Instant::now(),
        })
    }
}

/// WebSocket 通用消息 (保留用于未来扩展)
#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum WsMessage {
    MarkPrice(MarkPriceUpdate),
    BookTicker(BookTicker),
    AccountUpdate(AccountUpdate),
    OrderUpdate(OrderTradeUpdate),
    Other(serde_json::Value),
}

#[allow(dead_code)]
impl WsMessage {
    pub fn event_type(&self) -> &str {
        match self {
            WsMessage::MarkPrice(m) => &m.e,
            WsMessage::BookTicker(m) => &m.e,
            WsMessage::AccountUpdate(m) => &m.e,
            WsMessage::OrderUpdate(m) => &m.e,
            WsMessage::Other(v) => v.get("e").and_then(|e| e.as_str()).unwrap_or("unknown"),
        }
    }
}

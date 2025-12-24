use crate::domain::types::{OrderId, Price, Quantity, Rate, Timestamp};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 交易所枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Exchange {
    Binance,
    OKX,
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exchange::Binance => write!(f, "Binance"),
            Exchange::OKX => write!(f, "OKX"),
        }
    }
}

/// 交易方向
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Long => write!(f, "Long"),
            Side::Short => write!(f, "Short"),
        }
    }
}

/// 订单有效期类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good Till Cancel
    GTC,
    /// Immediate or Cancel
    IOC,
    /// Fill or Kill
    FOK,
    /// Maker Only
    PostOnly,
}

/// 订单类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit { price: Price, tif: TimeInForce },
}

/// 订单状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    PartiallyFilled { filled: Quantity },
    Filled,
    Cancelled,
    Rejected { reason: String },
}

/// 统一交易对符号
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol {
    pub base: String,
    pub quote: String,
}

impl Symbol {
    pub fn new(base: impl Into<String>, quote: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            quote: quote.into(),
        }
    }

    /// 规范化名称 (e.g., "BTC_USDT")
    pub fn canonical(&self) -> String {
        format!("{}_{}", self.base, self.quote)
    }

    /// 从规范化名称解析
    pub fn from_canonical(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('_').collect();
        if parts.len() == 2 {
            Some(Symbol {
                base: parts[0].to_string(),
                quote: parts[1].to_string(),
            })
        } else {
            None
        }
    }

    /// 转换为 Binance 格式 (e.g., "BTCUSDT")
    pub fn to_binance(&self) -> String {
        format!("{}{}", self.base, self.quote)
    }

    /// 从 Binance 格式解析
    pub fn from_binance(s: &str) -> Option<Self> {
        const KNOWN_QUOTES: [&str; 4] = ["USDT", "BUSD", "USDC", "USD"];

        for quote in KNOWN_QUOTES {
            if s.ends_with(quote) {
                let base = &s[..s.len() - quote.len()];
                return Some(Symbol {
                    base: base.to_string(),
                    quote: quote.to_string(),
                });
            }
        }
        None
    }

    /// 转换为 OKX 格式 (e.g., "BTC-USDT-SWAP")
    pub fn to_okx(&self) -> String {
        format!("{}-{}-SWAP", self.base, self.quote)
    }

    /// 从 OKX 格式解析
    pub fn from_okx(inst_id: &str) -> Option<Self> {
        let parts: Vec<&str> = inst_id.split('-').collect();
        if parts.len() == 3 && parts[2] == "SWAP" {
            Some(Symbol {
                base: parts[0].to_string(),
                quote: parts[1].to_string(),
            })
        } else {
            None
        }
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.canonical())
    }
}

/// 资金费率
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub rate: Rate,
    pub next_settle_time: Timestamp,
}

impl FundingRate {
    /// 年化费率 (假设每天3次结算)
    pub fn annualized_rate(&self) -> Rate {
        self.rate * 3.0 * 365.0
    }
}

/// 仓位信息
#[derive(Debug, Clone)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub size: Quantity,
    pub entry_price: Price,
    pub leverage: u32,
    pub unrealized_pnl: f64,
    pub mark_price: Price,
}

impl Position {
    /// 计算持仓名义价值
    pub fn notional_value(&self) -> f64 {
        self.mark_price * self.size.abs()
    }

    /// 判断是否空仓
    pub fn is_empty(&self) -> bool {
        self.size == 0.0
    }

    /// 创建空仓位
    pub fn empty(exchange: Exchange, symbol: Symbol) -> Self {
        Self {
            exchange,
            symbol,
            side: Side::Long,
            size: 0.0,
            entry_price: 0.0,
            leverage: 1,
            unrealized_pnl: 0.0,
            mark_price: 0.0,
        }
    }
}

/// 账户余额
#[derive(Debug, Clone)]
pub struct Balance {
    pub exchange: Exchange,
    pub asset: String,
    pub available: f64,
    pub frozen: f64,
}

impl Balance {
    pub fn total(&self) -> f64 {
        self.available + self.frozen
    }
}

/// Best Bid Offer (L1 行情)
#[derive(Debug, Clone)]
pub struct BBO {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub bid_price: Price,
    pub bid_qty: Quantity,
    pub ask_price: Price,
    pub ask_qty: Quantity,
    pub timestamp: Timestamp,
}

impl BBO {
    /// 计算买卖价差
    pub fn spread(&self) -> Price {
        self.ask_price - self.bid_price
    }

    /// 计算中间价
    pub fn mid_price(&self) -> Price {
        (self.bid_price + self.ask_price) / 2.0
    }
}

/// 订单
#[derive(Debug, Clone)]
pub struct Order {
    pub id: OrderId,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}

/// 订单更新事件
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub status: OrderStatus,
    pub filled_quantity: Quantity,
    pub avg_price: Option<Price>,
    pub timestamp: Timestamp,
}

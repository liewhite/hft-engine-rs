use crate::domain::types::{OrderId, Price, Quantity, Rate, Timestamp};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
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
    /// 结算间隔 (小时)，用于日化费率计算
    pub settle_interval_hours: f64,
}

impl FundingRate {
    /// 日化费率 = rate * (24 / settle_interval_hours)
    pub fn daily_rate(&self) -> Rate {
        if self.settle_interval_hours <= 0.0 {
            return 0.0;
        }
        self.rate * (24.0 / self.settle_interval_hours)
    }

    /// 年化费率 = daily_rate * 365
    pub fn annualized_rate(&self) -> Rate {
        self.daily_rate() * 365.0
    }
}

/// 仓位信息
///
/// size 为正表示多头，为负表示空头
#[derive(Debug, Clone)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 仓位数量：正数为多头，负数为空头
    pub size: Quantity,
    pub entry_price: Price,
    pub leverage: u32,
    pub unrealized_pnl: f64,
    pub mark_price: Price,
}

impl Position {
    /// 仓位比较的浮点精度阈值
    pub const EPSILON: f64 = 1e-10;

    /// 计算持仓名义价值
    pub fn notional_value(&self) -> f64 {
        self.mark_price * self.size.abs()
    }

    /// 判断是否空仓 (使用 epsilon 比较避免浮点精度问题)
    pub fn is_empty(&self) -> bool {
        self.size.abs() < Self::EPSILON
    }

    /// 获取持仓方向 (根据 size 符号)
    ///
    /// 注意: 空仓 (size == 0) 时返回 Side::Long，调用前应先检查 is_empty()
    pub fn side(&self) -> Side {
        if self.size >= 0.0 {
            Side::Long
        } else {
            Side::Short
        }
    }

    /// 创建空仓位
    pub fn empty(exchange: Exchange, symbol: Symbol) -> Self {
        Self {
            exchange,
            symbol,
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

/// 交易对元数据
#[derive(Debug, Clone)]
pub struct SymbolMeta {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 价格精度 (最小价格变动单位)
    pub price_step: f64,
    /// 数量精度 (最小数量变动单位)
    pub size_step: f64,
    /// 最小下单数量
    pub min_order_size: f64,
    /// 合约乘数 (OKX: cval, Binance: 1.0)
    ///
    /// 表示每张合约对应的币本位数量
    /// - OKX ETH: cval=0.1, 下单 qty=1 表示 0.1 ETH
    /// - Binance: 直接按币的数量下单, 等效于 cval=1.0
    pub contract_size: f64,
}

impl SymbolMeta {
    /// 检查元数据是否有效 (所有精度值 > 0)
    pub fn is_valid(&self) -> bool {
        self.price_step > 0.0 && self.size_step > 0.0 && self.contract_size > 0.0
    }

    /// 将币本位数量转换为下单数量
    ///
    /// 例如: 想下 0.5 ETH, OKX cval=0.1, 则返回 5 (张)
    pub fn coin_to_qty(&self, coin_amount: f64) -> f64 {
        coin_amount / self.contract_size
    }

    /// 将下单数量转换为币本位数量
    ///
    /// 例如: 下单 5 张, OKX cval=0.1, 则返回 0.5 ETH
    pub fn qty_to_coin(&self, qty: f64) -> f64 {
        qty * self.contract_size
    }

    /// 将价格调整到合法精度 (向下取整)
    pub fn round_price_down(&self, price: f64) -> f64 {
        Self::round_to_step(price, self.price_step, RoundingStrategy::ToNegativeInfinity)
    }

    /// 将价格调整到合法精度 (向上取整)
    pub fn round_price_up(&self, price: f64) -> f64 {
        Self::round_to_step(price, self.price_step, RoundingStrategy::ToPositiveInfinity)
    }

    /// 将数量调整到合法精度 (向下取整)
    pub fn round_size_down(&self, size: f64) -> f64 {
        Self::round_to_step(size, self.size_step, RoundingStrategy::ToNegativeInfinity)
    }

    /// 使用 Decimal 精确计算，按 step 取整
    fn round_to_step(value: f64, step: f64, strategy: RoundingStrategy) -> f64 {
        // 转换为 Decimal 进行精确计算
        let value_dec = Decimal::from_f64(value).unwrap_or_default();
        let step_dec = Decimal::from_f64(step).unwrap_or(Decimal::ONE);

        // 计算 tick 数 (value / step)，然后取整
        let ticks = value_dec / step_dec;
        let rounded_ticks = ticks.round_dp_with_strategy(0, strategy);

        // 乘回 step 得到结果
        let result = rounded_ticks * step_dec;
        result.to_f64().unwrap_or(value)
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
    pub client_order_id: Option<String>,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub status: OrderStatus,
    pub filled_quantity: Quantity,
    pub avg_price: Option<Price>,
    pub timestamp: Timestamp,
}

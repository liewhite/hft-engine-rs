use crate::domain::models::{Exchange, Symbol};
use crate::domain::types::{Price, Timestamp};
use std::fmt;

/// K线周期
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandleInterval {
    Min1,
    Min3,
    Min5,
    Min15,
    Min30,
    Hour1,
    Hour2,
    Hour4,
    Hour6,
    Hour12,
    Day1,
    Week1,
    Month1,
    Month3,
}

impl fmt::Display for CandleInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandleInterval::Min1 => write!(f, "1m"),
            CandleInterval::Min3 => write!(f, "3m"),
            CandleInterval::Min5 => write!(f, "5m"),
            CandleInterval::Min15 => write!(f, "15m"),
            CandleInterval::Min30 => write!(f, "30m"),
            CandleInterval::Hour1 => write!(f, "1H"),
            CandleInterval::Hour2 => write!(f, "2H"),
            CandleInterval::Hour4 => write!(f, "4H"),
            CandleInterval::Hour6 => write!(f, "6H"),
            CandleInterval::Hour12 => write!(f, "12H"),
            CandleInterval::Day1 => write!(f, "1D"),
            CandleInterval::Week1 => write!(f, "1W"),
            CandleInterval::Month1 => write!(f, "1M"),
            CandleInterval::Month3 => write!(f, "3M"),
        }
    }
}

/// K线数据
#[derive(Debug, Clone)]
pub struct Candle {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub interval: CandleInterval,
    /// K线开始时间 (毫秒)
    pub open_time: Timestamp,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    /// 交易量（张）
    pub volume: f64,
    /// true=已完结, false=实时更新中
    pub confirm: bool,
}

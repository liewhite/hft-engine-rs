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

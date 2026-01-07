use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// 交易所枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Exchange {
    Binance,
    OKX,
    Hyperliquid,
}

impl Exchange {
    /// 生成交易所特定的 client_order_id
    ///
    /// - Binance/Hyperliquid: "0x" + uuid (32位hex)
    /// - OKX: "x" + uuid (OKX 不允许以数字0开头)
    pub fn new_cli_order_id(&self) -> String {
        let uuid_hex = Uuid::new_v4().simple().to_string();
        match self {
            Exchange::OKX => format!("x{}", &uuid_hex[..31]),
            _ => format!("0x{}", uuid_hex),
        }
    }
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exchange::Binance => write!(f, "Binance"),
            Exchange::OKX => write!(f, "OKX"),
            Exchange::Hyperliquid => write!(f, "Hyperliquid"),
        }
    }
}

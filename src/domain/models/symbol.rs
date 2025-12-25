use serde::{Deserialize, Serialize};
use std::fmt;

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

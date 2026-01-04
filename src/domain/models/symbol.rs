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
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.canonical())
    }
}

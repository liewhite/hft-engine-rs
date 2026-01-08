use serde::{Deserialize, Serialize};
use std::fmt;

/// 统一交易对符号（只包含 base，quote 由交易所配置决定）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol {
    pub base: String,
}

impl Symbol {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.base)
    }
}

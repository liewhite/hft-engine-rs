//! Binance Symbol 格式转换

use crate::domain::Symbol;

/// 转换为 Binance 格式 (e.g., "BTCUSDT")
pub fn to_binance(symbol: &Symbol, quote: &str) -> String {
    format!("{}{}", symbol, quote)
}

/// 从 Binance 格式解析 Symbol
pub fn from_binance(s: &str, quote: &str) -> Option<Symbol> {
    s.strip_suffix(quote).map(|base| base.to_string())
}

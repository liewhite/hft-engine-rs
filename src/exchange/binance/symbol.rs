//! Binance Symbol 格式转换

use crate::domain::Symbol;

/// 转换为 Binance 格式 (e.g., "BTCUSDT")
pub fn to_binance(symbol: &Symbol) -> String {
    format!("{}{}", symbol.base, symbol.quote)
}

/// 从 Binance 格式解析 Symbol
pub fn from_binance(s: &str) -> Option<Symbol> {
    const KNOWN_QUOTES: [&str; 4] = ["USDT", "BUSD", "USDC", "USD"];

    for quote in KNOWN_QUOTES {
        if s.ends_with(quote) {
            let base = &s[..s.len() - quote.len()];
            return Some(Symbol::new(base, quote));
        }
    }
    None
}

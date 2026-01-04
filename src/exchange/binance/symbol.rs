//! Binance Symbol 格式转换

use crate::domain::Symbol;

/// Binance Symbol 格式扩展
pub trait BinanceSymbol {
    /// 转换为 Binance 格式 (e.g., "BTCUSDT")
    fn to_binance(&self) -> String;
}

impl BinanceSymbol for Symbol {
    fn to_binance(&self) -> String {
        format!("{}{}", self.base, self.quote)
    }
}

/// 从 Binance 格式解析 Symbol
pub fn parse_binance_symbol(s: &str) -> Option<Symbol> {
    const KNOWN_QUOTES: [&str; 4] = ["USDT", "BUSD", "USDC", "USD"];

    for quote in KNOWN_QUOTES {
        if s.ends_with(quote) {
            let base = &s[..s.len() - quote.len()];
            return Some(Symbol::new(base, quote));
        }
    }
    None
}

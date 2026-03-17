//! Hyperliquid Symbol 格式转换
//!
//! 处理 dex 前缀: 策略统一使用 base symbol (e.g., "AAPL")，
//! HL 内部使用 "{dex}:{symbol}" (e.g., "xyz:AAPL")

use crate::domain::Symbol;

/// 转换为 Hyperliquid 内部格式
///
/// dex 为空时直接返回 symbol，否则添加 "{dex}:" 前缀
pub fn to_hyperliquid(symbol: &Symbol, _quote: &str, dex: &str) -> String {
    if dex.is_empty() {
        symbol.clone()
    } else {
        format!("{}:{}", dex, symbol)
    }
}

/// 从 Hyperliquid 内部格式解析为 base symbol
///
/// 去掉 "{dex}:" 前缀 (e.g., "xyz:AAPL" → "AAPL")
pub fn from_hyperliquid(coin: &str) -> Symbol {
    coin.split(':').last().unwrap_or(coin).to_string()
}

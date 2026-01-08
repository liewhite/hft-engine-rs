//! Hyperliquid Symbol 格式转换
//!
//! Hyperliquid 支持多种结算币种 (USDC, USDE 等)

use crate::domain::Symbol;

/// 转换为 Hyperliquid 格式
/// - USDC: "BTC" (默认格式)
/// - USDE: 待确认格式
pub fn to_hyperliquid(symbol: &Symbol, _quote: &str) -> String {
    // Hyperliquid 当前只使用 base 名称
    // 不同 quote 的合约可能通过不同的 API 端点或参数区分
    symbol.base.clone()
}

/// 从 Hyperliquid 格式解析 Symbol
/// coin: 币种名 (e.g., "BTC", "ETH")
pub fn from_hyperliquid(coin: &str) -> Symbol {
    Symbol::new(coin)
}

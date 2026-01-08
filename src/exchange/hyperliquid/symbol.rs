//! Hyperliquid Symbol 格式转换
//!
//! 只处理主流永续合约（无冒号的 symbol）

use crate::domain::Symbol;

/// 转换为 Hyperliquid 格式
/// Hyperliquid 直接使用 coin 名称 (e.g., "BTC", "ETH")
pub fn to_hyperliquid(symbol: &Symbol, _quote: &str) -> String {
    symbol.clone()
}

/// 从 Hyperliquid 格式解析 Symbol
/// coin: 币种名 (e.g., "BTC", "ETH")
pub fn from_hyperliquid(coin: &str) -> Symbol {
    coin.to_string()
}

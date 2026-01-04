//! Hyperliquid Symbol 格式转换

use crate::domain::Symbol;

/// 转换为 Hyperliquid 格式 (e.g., "BTC")
/// Hyperliquid 永续合约使用币种名，默认 USDC 结算
pub fn to_hyperliquid(symbol: &Symbol) -> String {
    symbol.base.clone()
}

/// 从 Hyperliquid 格式解析 Symbol
/// coin: 币种名 (e.g., "BTC", "ETH")
/// Hyperliquid 永续合约默认 USDC 结算
pub fn from_hyperliquid(coin: &str) -> Symbol {
    Symbol::new(coin, "USDC")
}

//! Hyperliquid Symbol 格式转换
//!
//! Hyperliquid 实际使用 USDC 结算，但系统内部统一用 USDT 作为标准 quote。
//! 转换时自动处理 USDT <-> USDC 映射。

use crate::domain::Symbol;

/// 转换为 Hyperliquid 格式 (e.g., "BTC")
/// Hyperliquid 永续合约使用币种名，默认 USDC 结算
pub fn to_hyperliquid(symbol: &Symbol) -> String {
    symbol.base.clone()
}

/// 从 Hyperliquid 格式解析 Symbol
/// coin: 币种名 (e.g., "BTC", "ETH")
/// Hyperliquid 实际用 USDC 结算，但返回 USDT 以统一系统内部表示
pub fn from_hyperliquid(coin: &str) -> Symbol {
    Symbol::new(coin, "USDT")
}

//! Hyperliquid Symbol 格式转换

use crate::domain::Symbol;

/// Hyperliquid Symbol 格式扩展
pub trait HyperliquidSymbol {
    /// 转换为 Hyperliquid 格式 (e.g., "BTC")
    /// Hyperliquid 永续合约使用币种名，默认 USDC 结算
    fn to_hyperliquid(&self) -> String;
}

impl HyperliquidSymbol for Symbol {
    fn to_hyperliquid(&self) -> String {
        self.base.clone()
    }
}

/// 从 Hyperliquid 格式解析 Symbol
/// coin: 币种名 (e.g., "BTC", "ETH")
/// Hyperliquid 永续合约默认 USDC 结算
pub fn parse_hyperliquid_symbol(coin: &str) -> Symbol {
    Symbol::new(coin, "USDC")
}

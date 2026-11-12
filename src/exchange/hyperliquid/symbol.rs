//! Hyperliquid Symbol 格式转换
//!
//! 处理 dex 前缀: 策略统一使用 base symbol (e.g., "AAPL")，
//! HL 内部使用 "{dex}:{symbol}" (e.g., "xyz:AAPL")。出向（symbol → coin）只走品种目录
//! `markets::HlMarkets`，不在这里拼：拼出来的 coin 不保证存在、也不知道在哪个 dex。

use crate::domain::Symbol;

/// 从 Hyperliquid 内部格式解析为 base symbol
///
/// 去掉 "{dex}:" 前缀 (e.g., "xyz:AAPL" → "AAPL")
pub fn from_hyperliquid(coin: &str) -> Symbol {
    coin.split(':').last().unwrap_or(coin).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_stripped() {
        assert_eq!(from_hyperliquid("xyz:AAPL"), "AAPL");
        assert_eq!(from_hyperliquid("BTC"), "BTC");
    }
}

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

/// 该 coin 是否属于本 client 接入的 dex。
///
/// HL 的**账户级**接口（如 `userFunding`）不按 dex 过滤，一次返回该地址在所有 perp dex
/// 上的记录。若只用 [`from_hyperliquid`] 取冒号后的 base symbol，默认 dex 的 "AAPL" 与
/// xyz dex 的 "xyz:AAPL" 会被认成同一个 symbol —— 结果是把别的 dex 的资费算进本策略。
/// 故账户级数据入口必须先按 dex 归属过滤。
pub fn belongs_to_dex(coin: &str, dex: &str) -> bool {
    match coin.split_once(':') {
        Some((prefix, _)) => prefix == dex,
        None => dex.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dex_owns_only_unprefixed_coins() {
        assert!(belongs_to_dex("ETH", ""));
        assert!(!belongs_to_dex("xyz:AAPL", ""));
    }

    #[test]
    fn named_dex_owns_only_its_own_prefix() {
        assert!(belongs_to_dex("xyz:AAPL", "xyz"));
        assert!(!belongs_to_dex("abc:AAPL", "xyz"));
        // 默认 dex 的裸 coin 不属于任何具名 dex —— 这正是同名标的会串味的那一种
        assert!(!belongs_to_dex("AAPL", "xyz"));
    }

    #[test]
    fn round_trip_between_base_symbol_and_wire_coin() {
        let symbol = "AAPL".to_string();
        let wire = to_hyperliquid(&symbol, "USDC", "xyz");
        assert_eq!(wire, "xyz:AAPL");
        assert!(belongs_to_dex(&wire, "xyz"));
        assert_eq!(from_hyperliquid(&wire), symbol);
    }
}

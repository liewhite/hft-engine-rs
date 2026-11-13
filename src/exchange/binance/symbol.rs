//! Binance 合约写法 ↔ 内部 symbol 的唯一映射处
//!
//! 引擎只接入**配置计价币的永续**（quote=USDT 时即 `{base}USDT`）。两个方向按计价币精确匹配、
//! 互为逆映射：同账户里的 `BTCUSDC`、交割合约 `BTCUSDT_251226` 都映射不到 —— 它们是别的品种。

use crate::domain::Symbol;

/// 转换为 Binance 格式 (e.g., "BTCUSDT")
pub fn to_binance(symbol: &Symbol, quote: &str) -> String {
    format!("{}{}", symbol, quote)
}

/// [`to_binance`] 的逆：只认 `{base}{quote}`，其余返回 `None`。
pub fn from_binance(s: &str, quote: &str) -> Option<Symbol> {
    let base = s.strip_suffix(quote)?;
    (!base.is_empty()).then(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_configured_quote_perpetual_maps() {
        assert_eq!(from_binance("BTCUSDT", "USDT").as_deref(), Some("BTC"));
        assert_eq!(
            from_binance("1000PEPEUSDT", "USDT").as_deref(),
            Some("1000PEPE")
        );
        for other in ["BTCUSDC", "BTCUSDT_251226", "USDT", "BTCUSD_PERP"] {
            assert_eq!(from_binance(other, "USDT"), None, "{other}");
        }
        assert_eq!(from_binance("BTCUSDC", "USDC").as_deref(), Some("BTC"));
    }
}

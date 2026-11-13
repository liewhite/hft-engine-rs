//! OKX 合约写法 ↔ 内部 symbol 的唯一映射处
//!
//! 引擎只接入**配置计价币的永续**（quote=USDT 时即 `{base}-USDT-SWAP`）。两个方向都按计价币
//! 精确匹配，互为逆映射：同账户里的 `BTC-USDC-SWAP`、币本位 `BTC-USD-SWAP`、交割合约都映射
//! 不到 —— 它们是别的品种，绝不能当成 BTC（否则会按 USDT 合约的每张币数折算、串进 BTC 的持仓）。

use crate::domain::Symbol;

/// 转换为 OKX 格式 (e.g., "BTC-USDT-SWAP")
pub fn to_okx(symbol: &Symbol, quote: &str) -> String {
    format!("{}-{}-SWAP", symbol, quote)
}

/// 转换为 OKX 指数格式 (e.g., "BTC-USDT")
pub fn to_okx_index(symbol: &Symbol, quote: &str) -> String {
    format!("{}-{}", symbol, quote)
}

/// [`to_okx`] 的逆：只认 `{base}-{quote}-SWAP`，其余（别的计价币、币本位、交割）返回 `None`。
pub fn from_okx(inst_id: &str, quote: &str) -> Option<Symbol> {
    inst_id
        .strip_suffix("-SWAP")
        .and_then(|rest| strip_quote(rest, quote))
}

/// [`to_okx_index`] 的逆：只认 `{base}-{quote}`。
pub fn from_okx_index(inst_id: &str, quote: &str) -> Option<Symbol> {
    strip_quote(inst_id, quote)
}

fn strip_quote(s: &str, quote: &str) -> Option<Symbol> {
    let base = s.strip_suffix(quote)?.strip_suffix('-')?;
    (!base.is_empty() && !base.contains('-')).then(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_configured_quote_perpetual_maps() {
        assert_eq!(from_okx("BTC-USDT-SWAP", "USDT").as_deref(), Some("BTC"));
        for other in [
            "BTC-USDC-SWAP",
            "BTC-USD-SWAP",
            "BTC-USDT-251226",
            "BTC-USDT",
            "-USDT-SWAP",
            "A-B-USDT-SWAP",
        ] {
            assert_eq!(from_okx(other, "USDT"), None, "{other}");
        }
        assert_eq!(from_okx("BTC-USDC-SWAP", "USDC").as_deref(), Some("BTC"));
    }

    #[test]
    fn round_trip() {
        let s: Symbol = "PEPE".into();
        assert_eq!(from_okx(&to_okx(&s, "USDT"), "USDT"), Some(s.clone()));
        assert_eq!(from_okx_index(&to_okx_index(&s, "USDT"), "USDT"), Some(s));
        assert_eq!(from_okx_index("BTC-USD", "USDT"), None);
    }
}

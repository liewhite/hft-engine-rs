//! OKX Symbol 格式转换

use crate::domain::Symbol;

/// 转换为 OKX 格式 (e.g., "BTC-USDT-SWAP")
pub fn to_okx(symbol: &Symbol) -> String {
    format!("{}-{}-SWAP", symbol.base, symbol.quote)
}

/// 转换为 OKX 指数格式 (e.g., "BTC-USDT")
pub fn to_okx_index(symbol: &Symbol) -> String {
    format!("{}-{}", symbol.base, symbol.quote)
}

/// 从 OKX 格式解析 Symbol
pub fn from_okx(inst_id: &str) -> Option<Symbol> {
    let parts: Vec<&str> = inst_id.split('-').collect();
    if parts.len() == 3 && parts[2] == "SWAP" {
        Some(Symbol::new(parts[0], parts[1]))
    } else {
        None
    }
}

/// 从 OKX 指数格式解析 Symbol (e.g., "BTC-USDT")
pub fn from_okx_index(inst_id: &str) -> Option<Symbol> {
    let parts: Vec<&str> = inst_id.split('-').collect();
    if parts.len() == 2 {
        Some(Symbol::new(parts[0], parts[1]))
    } else {
        None
    }
}

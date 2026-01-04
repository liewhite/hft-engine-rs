//! OKX Symbol 格式转换

use crate::domain::Symbol;

/// OKX Symbol 格式扩展
pub trait OkxSymbol {
    /// 转换为 OKX 格式 (e.g., "BTC-USDT-SWAP")
    fn to_okx(&self) -> String;
}

impl OkxSymbol for Symbol {
    fn to_okx(&self) -> String {
        format!("{}-{}-SWAP", self.base, self.quote)
    }
}

/// 从 OKX 格式解析 Symbol
pub fn parse_okx_symbol(inst_id: &str) -> Option<Symbol> {
    let parts: Vec<&str> = inst_id.split('-').collect();
    if parts.len() == 3 && parts[2] == "SWAP" {
        Some(Symbol::new(parts[0], parts[1]))
    } else {
        None
    }
}

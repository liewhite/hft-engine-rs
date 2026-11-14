//! 本引擎订单号（client_order_id）的格式。
//!
//! 格式是各所的线路知识（长度、字符集、开头字符的限制），归各所适配层：每所声明一个
//! [`CliOrderIdFormat`] 常量（如 `binance::CLIENT_ORDER_IDS`），装配时把生成函数随 `ExchangeSetup`
//! 交给框架；框架只拿不透明的字符串，不解读格式。
//!
//! 不用订单号分辨「是不是本引擎的单」：账户归本引擎独占（见 `docs/architecture.md`「账户独占」）。

use uuid::Uuid;

/// 一个所的本引擎订单号格式：`prefix` + UUID 十六进制的前 `hex_len` 位
#[derive(Debug, Clone, Copy)]
pub struct CliOrderIdFormat {
    prefix: &'static str,
    hex_len: usize,
}

impl CliOrderIdFormat {
    pub(crate) const fn new(prefix: &'static str, hex_len: usize) -> Self {
        assert!(hex_len > 0 && hex_len <= 32);
        Self { prefix, hex_len }
    }

    /// 生成一个新订单号（随机 UUID，跨重启唯一）
    pub fn generate(&self) -> String {
        let hex = Uuid::new_v4().simple().to_string();
        format!("{}{}", self.prefix, &hex[..self.hex_len])
    }
}

#[cfg(test)]
mod tests {
    use crate::exchange::{binance, hyperliquid, okx};

    /// 各所长度与格式（交易所的硬限制，改了下单会被拒）
    #[test]
    fn wire_formats() {
        let hl = hyperliquid::CLIENT_ORDER_IDS.generate();
        assert!(
            hl.starts_with("0x") && hl.len() == 34,
            "HL cloid 必须是 0x + 32 hex: {hl}"
        );
        let bn = binance::CLIENT_ORDER_IDS.generate();
        assert!(bn.starts_with("0x") && bn.len() == 34, "{bn}");
        let okx = okx::CLIENT_ORDER_IDS.generate();
        assert!(
            okx.starts_with('x') && okx.len() == 32,
            "OKX 不能以数字开头、至多 32 位: {okx}"
        );
    }
}

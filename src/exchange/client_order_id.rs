//! 本引擎订单号（client_order_id）的格式：生成与归属判定。
//!
//! 订单号的格式是各所的线路知识（长度、字符集、开头字符的限制），归各所适配层：每所声明一个
//! [`CliOrderIdFormat`] 常量（如 `binance::CLIENT_ORDER_IDS`），框架核心只拿不透明的字符串：
//! - **出向**：装配时把该所的生成函数随 `ExchangeSetup` 交给框架，框架给策略的单分配订单号时调用；
//! - **入向**：适配层把交易所回报解析成领域事件时，经 [`CliOrderIdFormat::claim`] 判定归属 ——
//!   领域事件上的 `client_order_id` 只给本引擎的单，别人的单（人工、其他程序、强平等系统单）一律
//!   `None`。核心和策略见到 `Some` 就是本引擎的单，不再自己判断。
//!
//! 前缀不止满足格式，更是**归属标记**：同一账户上可能还有别人的单，启动期撤遗留挂单时撤错别人的
//! 单不可接受。前缀认不出本引擎的另一个实例（前缀相同）；分桶部署下各实例负责的币不重叠，可接受。

use uuid::Uuid;

/// 一个所的本引擎订单号格式：`prefix` + UUID 十六进制的前 `hex_len` 位
#[derive(Debug, Clone, Copy)]
pub struct CliOrderIdFormat {
    prefix: &'static str,
    hex_len: usize,
}

impl CliOrderIdFormat {
    pub(crate) const fn new(prefix: &'static str, hex_len: usize) -> Self {
        assert!(!prefix.is_empty() && hex_len > 0 && hex_len <= 32);
        Self { prefix, hex_len }
    }

    /// 生成一个新订单号（随机 UUID，跨重启唯一）
    pub fn generate(&self) -> String {
        let hex = Uuid::new_v4().simple().to_string();
        format!("{}{}", self.prefix, &hex[..self.hex_len])
    }

    /// 该订单号是否由本引擎生成（只看前缀）
    pub fn owns(&self, client_order_id: &str) -> bool {
        client_order_id.starts_with(self.prefix)
    }

    /// 入向回报的订单号归属判定：本引擎的单原样带上，没有订单号或别人的单给 `None`
    pub fn claim(&self, raw: Option<&str>) -> Option<String> {
        raw.filter(|id| self.owns(id)).map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use crate::exchange::{binance, hyperliquid, ibkr, okx};

    fn all() -> [(&'static str, super::CliOrderIdFormat); 4] {
        [
            ("Binance", binance::CLIENT_ORDER_IDS),
            ("OKX", okx::CLIENT_ORDER_IDS),
            ("Hyperliquid", hyperliquid::CLIENT_ORDER_IDS),
            ("IBKR", ibkr::CLIENT_ORDER_IDS),
        ]
    }

    /// 生成的订单号认得出是自己的（否则启动期撤不掉遗留挂单，策略会在旁边重复挂单）
    #[test]
    fn generated_ids_are_owned() {
        for (name, f) in all() {
            let id = f.generate();
            assert!(f.owns(&id), "{name}: 认不出自己生成的 {id}");
            assert_eq!(f.claim(Some(&id)).as_deref(), Some(id.as_str()), "{name}");
        }
    }

    /// 各所长度与格式（交易所的硬限制，改了下单会被拒）
    #[test]
    fn wire_formats() {
        let hl = hyperliquid::CLIENT_ORDER_IDS.generate();
        assert!(hl.starts_with("0x") && hl.len() == 34, "HL cloid 必须是 0x + 32 hex: {hl}");
        let bn = binance::CLIENT_ORDER_IDS.generate();
        assert!(bn.starts_with("0x") && bn.len() == 34, "{bn}");
        let okx = okx::CLIENT_ORDER_IDS.generate();
        assert!(okx.starts_with('x') && okx.len() == 32, "OKX 不能以数字开头、至多 32 位: {okx}");
        let ib = ibkr::CLIENT_ORDER_IDS.generate();
        assert!(ib.starts_with("ib") && ib.len() == 32, "{ib}");
    }

    /// 各所前缀互不误判（BN/HL 共用 `0x`：两者账户不会混在一起，无害）
    #[test]
    fn prefixes_do_not_cross_claim() {
        let okx_id = okx::CLIENT_ORDER_IDS.generate();
        let ibkr_id = ibkr::CLIENT_ORDER_IDS.generate();
        let binance_id = binance::CLIENT_ORDER_IDS.generate();
        assert!(!binance::CLIENT_ORDER_IDS.owns(&okx_id));
        assert!(!binance::CLIENT_ORDER_IDS.owns(&ibkr_id));
        assert!(!okx::CLIENT_ORDER_IDS.owns(&binance_id));
        assert!(!okx::CLIENT_ORDER_IDS.owns(&ibkr_id));
        assert!(!ibkr::CLIENT_ORDER_IDS.owns(&binance_id));
        assert!(!ibkr::CLIENT_ORDER_IDS.owns(&okx_id));
    }

    /// 外部下单（人工 / 其他程序）与没有订单号的单不被认领
    #[test]
    fn foreign_ids_are_not_claimed() {
        for (name, f) in all() {
            for foreign in ["web_1a2b3c", "android_9f8e", "ios_x1", "", "electron_7"] {
                assert!(f.claim(Some(foreign)).is_none(), "{name} 误认领了外部订单 id {foreign:?}");
            }
            assert!(f.claim(None).is_none(), "{name}");
        }
    }
}

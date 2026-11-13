/// 资产常量
pub const USDT: &str = "USDT";

/// 数量类型 (持仓、订单、盘口挂单量、成交量)
///
/// # 不变量：domain 层的一切数量都是**币本位**
///
/// 策略与回测看到的 `Quantity` 永远是币的数量 (如 0.12 BTC)，**绝不是合约张数**。
/// 部分交易所 (OKX 的 SWAP/FUTURES) 用张数计量，其"张 -> 币"折算由**交易所适配层**
/// 在构造 domain 对象时一次性完成，绝不外泄给上层：
///
/// - WS 路径：codec 的构造方法在**签名上**要求 [`crate::domain::SymbolMeta`]
///   (如 `BboData::to_bbo`)，编译器保证不会漏折算
/// - REST 路径：由 [`crate::exchange::ExchangeClient`] 各实现内部折算后返回
/// - 反向 (币 -> 张) 只发生在下单出口 [`crate::exchange::ExchangeOrder::from_domain`]，张数只存在于
///   `ExchangeOrder` 内部，由各所 client 组装请求时读取
///
/// 违反此不变量的后果不是报错而是**静默算错**：OKX BTC 每张 0.01 币，漏折算即差 100 倍。
///
/// # 精确十进制
///
/// 数量与价格都是 [`Decimal`]：交易所回报是十进制字符串，原样解析不丢精度；三所持仓相加、相减、
/// 比大小，按步长取整，都是精确的，不需要任何容差。金额类（名义价值、手续费、资金费、权益）与
/// 比率类仍是 `f64` —— 它们只做统计与阈值判断，不决定下单数量。二者相乘得金额时用 [`to_f64`]。
pub type Quantity = Decimal;

/// 价格类型（精确十进制，见 [`Quantity`]）
pub type Price = Decimal;

pub use rust_decimal::Decimal;

/// 数量 / 价格 → `f64`（算金额、比率时用）。Decimal 转 f64 总能成功，取最近值。
pub fn to_f64(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().expect("Decimal 转 f64 不会失败")
}

/// `f64` → 数量 / 价格：只用于由金额、比率算出来的**意图量**（如「下 1000U」÷ 价格），结果必须
/// 再按品种步长取整（[`crate::domain::SymbolMeta`]）后才能下单。取最短十进制写法（0.29 → 0.29）。
/// 非有限值或超出 Decimal 范围返回 `None`。
pub fn from_f64(x: f64) -> Option<Decimal> {
    use rust_decimal::prelude::FromPrimitive;
    Decimal::from_f64(x)
}

/// 交易所下发的十进制字符串 → 数量 / 价格。**解析口径的唯一出处**（各所 codec、client 都经此）：
/// 普通小数，或科学计数法（`1e-5`，f64 时代能接受，防交易所偶发这种写法时整条回报解析失败）。
pub fn parse_decimal(raw: &str) -> Result<Decimal, String> {
    use std::str::FromStr;
    Decimal::from_str(raw)
        .or_else(|_| Decimal::from_scientific(raw))
        .map_err(|_| format!("数值非法: '{raw}'"))
}

/// 数量 / 价格 → 下单字符串：去尾零、不用科学计数法（`62562.60` → `62562.6`）。币安、OKX 用；
/// Hyperliquid 另有更严的写法（超过 8 位小数报错，签名机要求，见 `hyperliquid::transport::decimal_to_wire`）
pub fn to_wire(d: Decimal) -> String {
    d.normalize().to_string()
}

/// 严格为正的十进制：品种规格里的步长、最小下单量、合约乘数。只能经 [`Self::new`] 构造，
/// 所以拿它做除数、按它取整不用再判 0（交易所给的规格不合法，在加载处就被拒，不会流到下单路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositiveDecimal(Decimal);

impl PositiveDecimal {
    pub const ONE: Self = Self(Decimal::ONE);

    /// 不为正返回 `None`
    pub fn new(value: Decimal) -> Option<Self> {
        (value > Decimal::ZERO).then_some(Self(value))
    }

    pub fn get(self) -> Decimal {
        self.0
    }
}

impl std::fmt::Display for PositiveDecimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// 费率类型
pub type Rate = f64;

/// 订单 ID
pub type OrderId = String;

/// 时间戳 (毫秒级 Unix 时间戳)
pub type Timestamp = u64;

/// 获取当前时间戳 (毫秒)
pub fn now_ms() -> Timestamp {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time before Unix epoch")
        .as_millis() as u64
}
pub use rust_decimal_macros::dec;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_decimal_accepts_plain_and_scientific_exactly() {
        assert_eq!(parse_decimal("0.29").unwrap(), dec!(0.29));
        assert_eq!(parse_decimal("-12").unwrap(), dec!(-12));
        assert_eq!(parse_decimal("1e-5").unwrap(), dec!(0.00001));
        assert_eq!(parse_decimal("0E-8").unwrap(), dec!(0));
        assert!(parse_decimal("").is_err());
        assert!(parse_decimal("NaN").is_err());
    }

    #[test]
    fn wire_has_no_trailing_zeros_or_exponent() {
        assert_eq!(to_wire(dec!(62562.60)), "62562.6");
        assert_eq!(to_wire(dec!(0.00100000)), "0.001");
        assert_eq!(to_wire(dec!(4870)), "4870");
        assert_eq!(to_wire(dec!(0.00000001)), "0.00000001");
    }
}

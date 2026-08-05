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
/// - 反向 (币 -> 张) 只发生在下单出口，见 `StrategyRunner::convert_order`
///
/// 违反此不变量的后果不是报错而是**静默算错**：OKX BTC 每张 0.01 币，漏折算即差 100 倍。
pub type Quantity = f64;

/// 价格类型
pub type Price = f64;

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

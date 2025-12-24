/// 资产常量
pub const USDT: &str = "USDT";

/// 数量类型 (持仓数量、订单数量)
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

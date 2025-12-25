use serde::{Deserialize, Serialize};

/// 订单有效期类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good Till Cancel
    GTC,
    /// Immediate or Cancel
    IOC,
    /// Fill or Kill
    FOK,
    /// Maker Only
    PostOnly,
}

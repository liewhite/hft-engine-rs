use crate::domain::models::TimeInForce;
use crate::domain::types::Price;
use serde::{Deserialize, Serialize};

/// 订单类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit { price: Price, tif: TimeInForce },
}

use crate::domain::models::Exchange;

/// 账户余额
#[derive(Debug, Clone)]
pub struct Balance {
    pub exchange: Exchange,
    pub asset: String,
    pub available: f64,
    pub frozen: f64,
}

impl Balance {
    pub fn total(&self) -> f64 {
        self.available + self.frozen
    }
}

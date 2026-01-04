//! Binance ExchangeModule 实现

use super::{BinanceClient, BinanceCredentials};
use crate::domain::Exchange;
use crate::exchange::{ExchangeClient, ExchangeModule};
use std::sync::Arc;

/// Binance 交易所模块
pub struct BinanceModule {
    client: Arc<BinanceClient>,
}

impl BinanceModule {
    /// 创建新的 BinanceModule
    pub fn new(credentials: Option<BinanceCredentials>) -> Result<Self, crate::domain::ExchangeError> {
        let client = Arc::new(BinanceClient::new(credentials)?);
        Ok(Self { client })
    }

    /// 获取凭证引用（用于创建 Actor）
    pub fn credentials(&self) -> Option<&BinanceCredentials> {
        self.client.credentials()
    }
}

impl ExchangeModule for BinanceModule {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }
}

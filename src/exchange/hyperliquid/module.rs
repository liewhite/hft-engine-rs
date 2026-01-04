//! Hyperliquid ExchangeModule 实现

use super::{HyperliquidClient, HyperliquidCredentials};
use crate::domain::Exchange;
use crate::exchange::{ExchangeClient, ExchangeModule};
use std::sync::Arc;

/// Hyperliquid 交易所模块
pub struct HyperliquidModule {
    client: Arc<HyperliquidClient>,
}

impl HyperliquidModule {
    /// 创建新的 HyperliquidModule
    pub fn new(
        credentials: Option<HyperliquidCredentials>,
    ) -> Result<Self, crate::domain::ExchangeError> {
        let client = Arc::new(HyperliquidClient::new(credentials)?);
        Ok(Self { client })
    }

    /// 获取凭证引用（用于创建 Actor）
    pub fn credentials(&self) -> Option<&HyperliquidCredentials> {
        self.client.credentials()
    }
}

impl ExchangeModule for HyperliquidModule {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }
}

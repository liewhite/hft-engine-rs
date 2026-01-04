//! OKX ExchangeModule 实现

use super::{OkxClient, OkxCredentials};
use crate::domain::Exchange;
use crate::exchange::{ExchangeClient, ExchangeModule};
use std::sync::Arc;

/// OKX 交易所模块
pub struct OkxModule {
    client: Arc<OkxClient>,
}

impl OkxModule {
    /// 创建新的 OkxModule
    pub fn new(credentials: Option<OkxCredentials>) -> Result<Self, crate::domain::ExchangeError> {
        let client = Arc::new(OkxClient::new(credentials)?);
        Ok(Self { client })
    }

    /// 获取凭证引用（用于创建 Actor）
    pub fn credentials(&self) -> Option<&OkxCredentials> {
        self.client.credentials()
    }
}

impl ExchangeModule for OkxModule {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }
}

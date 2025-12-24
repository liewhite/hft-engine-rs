use crate::domain::{
    Balance, Exchange, ExchangeError, FundingRate, Order, OrderId, OrderUpdate, Position, Symbol,
    BBO,
};
use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Public 市场数据 Hub
pub struct PublicHubs {
    pub funding_rates: broadcast::Sender<FundingRate>,
    pub bbos: broadcast::Sender<BBO>,
}

impl PublicHubs {
    pub fn new(capacity: usize) -> Self {
        Self {
            funding_rates: broadcast::channel(capacity).0,
            bbos: broadcast::channel(capacity).0,
        }
    }
}

/// Private 账户数据 Hub
pub struct PrivateHubs {
    pub positions: broadcast::Sender<Position>,
    pub balances: broadcast::Sender<Balance>,
    pub order_updates: broadcast::Sender<OrderUpdate>,
}

impl PrivateHubs {
    pub fn new(capacity: usize) -> Self {
        Self {
            positions: broadcast::channel(capacity).0,
            balances: broadcast::channel(capacity).0,
            order_updates: broadcast::channel(capacity).0,
        }
    }
}

/// 交易所 WebSocket 连接 trait
#[async_trait]
pub trait ExchangeWebSocket: Send + Sync {
    fn exchange(&self) -> Exchange;

    /// 连接公共 WebSocket 并订阅市场数据
    async fn connect_public(
        &self,
        symbols: &[Symbol],
        cancel_token: CancellationToken,
    ) -> Result<PublicHubs, ExchangeError>;

    /// 连接私有 WebSocket 并订阅账户数据
    async fn connect_private(
        &self,
        cancel_token: CancellationToken,
    ) -> Result<PrivateHubs, ExchangeError>;
}

/// 交易所执行器 trait
#[async_trait]
pub trait ExchangeExecutor: Send + Sync {
    fn exchange(&self) -> Exchange;

    /// 下单
    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError>;

    /// 设置杠杆
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError>;
}


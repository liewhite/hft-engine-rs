use crate::domain::{
    Balance, Exchange, ExchangeError, FundingRate, Order, OrderId, OrderUpdate, Position, Symbol,
    BBO,
};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// 公共数据 Sink - 由消费者创建，按 Symbol 拆分
///
/// 消费者创建所需的 sender，生产者只负责推送数据
#[derive(Clone)]
pub struct PublicSinks {
    /// FundingRate per symbol
    pub funding_rates: HashMap<Symbol, broadcast::Sender<FundingRate>>,
    /// BBO per symbol
    pub bbos: HashMap<Symbol, broadcast::Sender<BBO>>,
}

impl PublicSinks {
    /// 创建指定 symbols 的 sinks
    pub fn new(symbols: &[Symbol], capacity: usize) -> Self {
        let mut funding_rates = HashMap::new();
        let mut bbos = HashMap::new();

        for symbol in symbols {
            funding_rates.insert(symbol.clone(), broadcast::channel(capacity).0);
            bbos.insert(symbol.clone(), broadcast::channel(capacity).0);
        }

        Self {
            funding_rates,
            bbos,
        }
    }

    /// 获取订阅的 symbols
    pub fn symbols(&self) -> Vec<Symbol> {
        self.funding_rates.keys().cloned().collect()
    }

    /// 订阅指定 symbol 的 FundingRate
    pub fn subscribe_funding_rate(&self, symbol: &Symbol) -> Option<broadcast::Receiver<FundingRate>> {
        self.funding_rates.get(symbol).map(|tx| tx.subscribe())
    }

    /// 订阅指定 symbol 的 BBO
    pub fn subscribe_bbo(&self, symbol: &Symbol) -> Option<broadcast::Receiver<BBO>> {
        self.bbos.get(symbol).map(|tx| tx.subscribe())
    }

    /// 获取所有 funding rate receivers (用于 metrics 订阅)
    pub fn funding_rate_receivers(&self) -> Vec<(Symbol, broadcast::Receiver<FundingRate>)> {
        self.funding_rates
            .iter()
            .map(|(s, tx)| (s.clone(), tx.subscribe()))
            .collect()
    }

    /// 获取所有 BBO receivers (用于 metrics 订阅)
    pub fn bbo_receivers(&self) -> Vec<(Symbol, broadcast::Receiver<BBO>)> {
        self.bbos
            .iter()
            .map(|(s, tx)| (s.clone(), tx.subscribe()))
            .collect()
    }
}

/// 私有数据 Sink - 由消费者创建，按 Symbol 拆分
#[derive(Clone)]
pub struct PrivateSinks {
    /// Position per symbol
    pub positions: HashMap<Symbol, broadcast::Sender<Position>>,
    /// Balance (per asset, 不按 symbol 拆分)
    pub balances: broadcast::Sender<Balance>,
    /// OrderUpdate per symbol
    pub order_updates: HashMap<Symbol, broadcast::Sender<OrderUpdate>>,
}

impl PrivateSinks {
    /// 创建指定 symbols 的 sinks
    pub fn new(symbols: &[Symbol], capacity: usize) -> Self {
        let mut positions = HashMap::new();
        let mut order_updates = HashMap::new();

        for symbol in symbols {
            positions.insert(symbol.clone(), broadcast::channel(capacity).0);
            order_updates.insert(symbol.clone(), broadcast::channel(capacity).0);
        }

        Self {
            positions,
            balances: broadcast::channel(capacity).0,
            order_updates,
        }
    }

    /// 获取订阅的 symbols
    pub fn symbols(&self) -> Vec<Symbol> {
        self.positions.keys().cloned().collect()
    }

    /// 订阅指定 symbol 的 Position
    pub fn subscribe_position(&self, symbol: &Symbol) -> Option<broadcast::Receiver<Position>> {
        self.positions.get(symbol).map(|tx| tx.subscribe())
    }

    /// 订阅 Balance (不按 symbol 拆分)
    pub fn subscribe_balance(&self) -> broadcast::Receiver<Balance> {
        self.balances.subscribe()
    }

    /// 订阅指定 symbol 的 OrderUpdate
    pub fn subscribe_order_update(&self, symbol: &Symbol) -> Option<broadcast::Receiver<OrderUpdate>> {
        self.order_updates.get(symbol).map(|tx| tx.subscribe())
    }

    /// 获取所有 position receivers (用于 metrics 订阅)
    pub fn position_receivers(&self) -> Vec<(Symbol, broadcast::Receiver<Position>)> {
        self.positions
            .iter()
            .map(|(s, tx)| (s.clone(), tx.subscribe()))
            .collect()
    }
}

/// 交易所 WebSocket 连接 trait
#[async_trait]
pub trait ExchangeWebSocket: Send + Sync {
    fn exchange(&self) -> Exchange;

    /// 连接公共 WebSocket 并订阅市场数据
    ///
    /// sinks 由消费者创建，生产者只负责推送数据到对应的 sender
    async fn connect_public(
        &self,
        sinks: PublicSinks,
        cancel_token: CancellationToken,
    ) -> Result<(), ExchangeError>;

    /// 连接私有 WebSocket 并订阅账户数据
    ///
    /// sinks 由消费者创建，生产者只负责推送数据到对应的 sender
    async fn connect_private(
        &self,
        sinks: PrivateSinks,
        cancel_token: CancellationToken,
    ) -> Result<(), ExchangeError>;
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

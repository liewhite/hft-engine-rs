use crate::domain::{Exchange, ExchangeError, OrderType, Quantity, Side, Symbol};
use crate::messaging::SymbolState;
use async_trait::async_trait;

/// 交易信号
#[derive(Debug, Clone)]
pub struct Signal {
    pub symbol: Symbol,
    pub actions: Vec<TradeAction>,
}

/// 交易动作
#[derive(Debug, Clone)]
pub struct TradeAction {
    pub exchange: Exchange,
    pub side: Side,
    pub quantity: Quantity,
    pub order_type: OrderType,
    pub reduce_only: bool,
}

impl TradeAction {
    /// 创建开仓动作
    pub fn open(exchange: Exchange, side: Side, quantity: Quantity, order_type: OrderType) -> Self {
        Self {
            exchange,
            side,
            quantity,
            order_type,
            reduce_only: false,
        }
    }

    /// 创建平仓动作
    pub fn close(exchange: Exchange, side: Side, quantity: Quantity, order_type: OrderType) -> Self {
        Self {
            exchange,
            side: side.opposite(),
            quantity,
            order_type,
            reduce_only: true,
        }
    }
}

/// 策略 trait
#[async_trait]
pub trait Strategy: Send + Sync {
    /// 评估当前状态，返回交易信号
    fn evaluate(&self, state: &SymbolState) -> Option<Signal>;

    /// 执行交易信号
    async fn execute(&self, signal: Signal) -> Result<(), ExchangeError>;
}

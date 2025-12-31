//! 交易所客户端统一抽象
//!
//! ExchangeClient trait 封装交易所 REST 交互
//! Sink traits 用于解耦 Actor 之间的数据流

use crate::domain::{Exchange, ExchangeError, Order, OrderId, Symbol, SymbolMeta};
use async_trait::async_trait;
use std::sync::Arc;

// ============================================================================
// 订阅类型
// ============================================================================

/// 订阅类型（仅 public 数据需要订阅）
///
/// Private 数据（Position/Balance/OrderUpdate/Equity）在 create_actor() 时自动处理
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SubscriptionKind {
    /// 资金费率
    FundingRate { symbol: Symbol },
    /// Best Bid/Offer
    BBO { symbol: Symbol },
}

impl SubscriptionKind {
    /// 获取订阅的 symbol
    pub fn symbol(&self) -> &Symbol {
        match self {
            SubscriptionKind::FundingRate { symbol } => symbol,
            SubscriptionKind::BBO { symbol } => symbol,
        }
    }
}

// ============================================================================
// 事件接收器
// ============================================================================

use crate::messaging::ExchangeEvent;

/// 交易所事件接收器
#[async_trait]
pub trait EventSink: Send + Sync + 'static {
    async fn send_event(&self, event: ExchangeEvent);
}

/// 为 Arc<T> 实现 EventSink (blanket impl)
#[async_trait]
impl<T: EventSink + ?Sized> EventSink for Arc<T> {
    async fn send_event(&self, event: ExchangeEvent) {
        self.as_ref().send_event(event).await;
    }
}

// ============================================================================
// ExchangeClient trait (仅 REST)
// ============================================================================

/// 交易所客户端统一接口
///
/// 仅封装交易所的 REST 交互，WebSocket Actor 由 ManagerActor 直接创建
#[async_trait]
pub trait ExchangeClient: Send + Sync + 'static {
    /// 获取交易所标识
    fn exchange(&self) -> Exchange;

    /// 获取所有交易对元数据
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError>;

    /// 获取指定交易对元数据
    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError>;

    /// 下单
    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError>;

    /// 设置杠杆
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError>;

    /// 获取账户净值 (balance + unrealized_pnl)
    async fn fetch_equity(&self) -> Result<f64, ExchangeError>;
}

// ============================================================================
// 订阅/取消订阅消息
// ============================================================================

/// 订阅消息
#[derive(Debug, Clone)]
pub struct Subscribe {
    pub kind: SubscriptionKind,
}

/// 取消订阅消息
#[derive(Debug, Clone)]
pub struct Unsubscribe {
    pub kind: SubscriptionKind,
}

/// WebSocket 错误
#[derive(Debug, Clone, thiserror::Error)]
pub enum WsError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Authentication failed: {0}")]
    AuthFailed(String),
    #[error("Server closed connection")]
    ServerClosed,
    #[error("Parse error: {0}")]
    ParseError(String),
}

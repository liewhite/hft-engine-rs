//! 交易所客户端统一抽象
//!
//! ExchangeClient trait 封装交易所 REST 交互
//! Sink traits 用于解耦 Actor 之间的数据流

use crate::domain::{
    Balance, Exchange, ExchangeError, FundingRate, Order, OrderId, OrderUpdate, Position, Symbol,
    SymbolMeta, BBO,
};
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

/// 公共数据类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublicDataType {
    /// 资金费率
    FundingRate,
    /// Best Bid/Offer
    BBO,
}

impl PublicDataType {
    /// 返回所有数据类型
    pub fn all() -> &'static [PublicDataType] {
        &[PublicDataType::FundingRate, PublicDataType::BBO]
    }

    /// 转换为 SubscriptionKind
    pub fn to_subscription_kind(self, symbol: Symbol) -> SubscriptionKind {
        match self {
            PublicDataType::FundingRate => SubscriptionKind::FundingRate { symbol },
            PublicDataType::BBO => SubscriptionKind::BBO { symbol },
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
// 信号接收器 (ExecutorActor → SignalProcessorActor)
// ============================================================================

use crate::strategy::Signal;

/// 信号接收器
#[async_trait]
pub trait SignalSink: Send + Sync + 'static {
    async fn send_signal(&self, signal: Signal);
}

/// 为 Arc<T> 实现 SignalSink (blanket impl)
#[async_trait]
impl<T: SignalSink + ?Sized> SignalSink for Arc<T> {
    async fn send_signal(&self, signal: Signal) {
        self.as_ref().send_signal(signal).await;
    }
}

// ============================================================================
// 解析后的消息（内部使用）
// ============================================================================

/// 解析后的 WebSocket 消息
#[derive(Debug)]
pub enum ParsedMessage {
    FundingRate {
        symbol: Symbol,
        rate: FundingRate,
    },
    BBO {
        symbol: Symbol,
        bbo: BBO,
    },
    Position {
        symbol: Symbol,
        position: Position,
    },
    Balance(Balance),
    OrderUpdate {
        symbol: Symbol,
        update: OrderUpdate,
    },
    Equity(f64),
    /// 订阅确认
    Subscribed,
    /// Ping/Pong
    Pong,
    /// 其他忽略的消息
    Ignored,
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

// ============================================================================
// WebSocket 连接相关
// ============================================================================

/// 连接 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub u64);

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
}

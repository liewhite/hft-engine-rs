//! 交易所客户端统一抽象
//!
//! ExchangeClient trait 封装交易所 REST 交互

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
    /// 标记价格
    MarkPrice { symbol: Symbol },
    /// 指数价格
    IndexPrice { symbol: Symbol },
}

impl SubscriptionKind {
    /// 获取订阅的 symbol
    pub fn symbol(&self) -> &Symbol {
        match self {
            SubscriptionKind::FundingRate { symbol } => symbol,
            SubscriptionKind::BBO { symbol } => symbol,
            SubscriptionKind::MarkPrice { symbol } => symbol,
            SubscriptionKind::IndexPrice { symbol } => symbol,
        }
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

// ============================================================================
// ExchangeModule trait (交易所模块抽象)
// ============================================================================

/// 交易所模块 trait
///
/// 封装交易所的客户端，使 ManagerActor 无需知道具体交易所类型。
/// spawn_actor 方法在各具体 Module 中实现（泛型方法，非 trait 方法）。
pub trait ExchangeModule: Send + Sync {
    /// 获取交易所标识
    fn exchange(&self) -> Exchange;

    /// 获取 REST 客户端
    fn client(&self) -> Arc<dyn ExchangeClient>;
}

// ============================================================================
// ExchangeActorOps trait (类型擦除的 Actor 操作接口)
// ============================================================================

/// 交易所 Actor 操作接口
///
/// 用于类型擦除，使 ManagerActor 可以用 `HashMap<Exchange, Box<dyn ExchangeActorOps>>`
/// 统一管理不同类型的交易所 Actor
#[async_trait]
pub trait ExchangeActorOps: Send + Sync {
    /// 获取 Actor ID（用于建立 link）
    fn actor_id(&self) -> kameo::actor::ActorId;
    /// 订阅
    async fn subscribe(&self, kind: SubscriptionKind) -> Result<(), String>;
    /// 取消订阅
    async fn unsubscribe(&self, kind: SubscriptionKind) -> Result<(), String>;
}

/// 为 ActorRef<A> 实现 ExchangeActorOps 的宏
///
/// 要求 A 实现 `Message<Subscribe>`, `Message<Unsubscribe>`
#[macro_export]
macro_rules! impl_exchange_actor_ops {
    ($($actor:ty),*) => {
        $(
            #[async_trait::async_trait]
            impl $crate::exchange::ExchangeActorOps for kameo::actor::ActorRef<$actor> {
                fn actor_id(&self) -> kameo::actor::ActorId {
                    self.id()
                }
                async fn subscribe(&self, kind: $crate::exchange::SubscriptionKind) -> Result<(), String> {
                    self.tell($crate::exchange::Subscribe { kind })
                        .send()
                        .await
                        .map_err(|e| e.to_string())
                }
                async fn unsubscribe(&self, kind: $crate::exchange::SubscriptionKind) -> Result<(), String> {
                    self.tell($crate::exchange::Unsubscribe { kind })
                        .send()
                        .await
                        .map_err(|e| e.to_string())
                }
            }
        )*
    };
}

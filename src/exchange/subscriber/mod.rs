//! WebSocket 订阅模块
//!
//! 定义订阅类型、市场数据、解析消息等核心类型

use crate::domain::{Balance, Exchange, FundingRate, OrderUpdate, Position, Symbol, BBO};

/// 订阅类型
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SubscriptionKind {
    /// 资金费率 (public)
    FundingRate { symbol: Symbol },
    /// Best Bid/Offer (public)
    BBO { symbol: Symbol },
    /// 私有数据 - 账户级别 (Position/Balance/OrderUpdate/Equity)
    Private,
}

/// 订阅器推送的统一市场数据
#[derive(Debug, Clone)]
pub enum MarketData {
    /// 资金费率更新
    FundingRate {
        exchange: Exchange,
        symbol: Symbol,
        rate: FundingRate,
    },
    /// BBO 更新
    BBO {
        exchange: Exchange,
        symbol: Symbol,
        bbo: BBO,
    },
    /// 仓位更新 (size 已转为 coin 单位)
    Position {
        exchange: Exchange,
        symbol: Symbol,
        position: Position,
    },
    /// 余额更新
    Balance {
        exchange: Exchange,
        balance: Balance,
    },
    /// 订单状态更新
    OrderUpdate {
        exchange: Exchange,
        symbol: Symbol,
        update: OrderUpdate,
    },
    /// 账户权益更新
    Equity {
        exchange: Exchange,
        value: f64,
    },
}

/// 订阅消息
pub struct Subscribe {
    pub kind: SubscriptionKind,
}

/// 取消订阅消息
pub struct Unsubscribe {
    pub kind: SubscriptionKind,
}

/// 订阅错误
#[derive(Debug, Clone, thiserror::Error)]
pub enum SubscribeError {
    #[error("WebSocket connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Subscribe request failed: {0}")]
    SubscribeFailed(String),
    #[error("Authentication failed: {0}")]
    AuthFailed(String),
}

/// 解析后的 WebSocket 消息
#[derive(Debug)]
pub enum ParsedMessage {
    FundingRate {
        symbol: Symbol,
        rate: FundingRate,
        /// 下次结算时间戳 (ms)，用于动态计算结算间隔 (Binance 专用)
        next_funding_time: Option<u64>,
    },
    BBO { symbol: Symbol, bbo: BBO },
    Position { symbol: Symbol, position: Position },
    Balance(Balance),
    OrderUpdate { symbol: Symbol, update: OrderUpdate },
    Equity(f64),
    /// 订阅确认
    Subscribed,
    /// Ping/Pong
    Pong,
    /// 其他忽略的消息
    Ignored,
}

/// 交易所配置 trait
///
/// 通过泛型参数化交易所差异，避免 if 分发
pub trait ExchangeConfig: Send + Sync + 'static {
    /// 交易所标识
    const EXCHANGE: Exchange;

    /// Public WebSocket URL
    const PUBLIC_WS_URL: &'static str;

    /// Private WebSocket URL
    const PRIVATE_WS_URL: &'static str;

    /// 单个 WebSocket 连接的最大订阅数
    const MAX_SUBSCRIPTIONS_PER_CONN: usize;

    /// 凭证类型
    type Credentials: Send + Sync + Clone;

    /// 构建订阅消息 JSON
    fn build_subscribe_msg(kinds: &[SubscriptionKind]) -> String;

    /// 构建取消订阅消息 JSON
    fn build_unsubscribe_msg(kinds: &[SubscriptionKind]) -> String;

    /// 解析 WebSocket 消息
    fn parse_message(raw: &str) -> Option<ParsedMessage>;

    /// 构建认证消息 (用于 private WebSocket)
    fn build_auth_msg(credentials: &Self::Credentials) -> String;

    /// 是否需要定期发送 ping 保活
    fn needs_ping() -> bool {
        false
    }

    /// Ping 消息内容
    fn ping_msg() -> Option<String> {
        None
    }
}

//! 交易所客户端统一抽象
//!
//! ExchangeClient trait 封装交易所 REST 交互

use crate::domain::{AccountInfo, CandleInterval, Exchange, ExchangeError, Order, OrderId, OrderUpdate, Position, Symbol, SymbolMeta};
use async_trait::async_trait;

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
    /// 公共成交印记 (成交流)
    ///
    /// # 统一口径：aggTrade（同一主动单在同一价位的多笔撮合合并为一条）
    ///
    /// 各所线路粒度并不相同，差异由**适配层吸收**，上层看到的永远是归集后的口径
    /// (与"数量一律币本位"同理，见 [`crate::domain::Quantity`])。各所实测情况：
    /// - Binance `aggTrade`：线路已归集 (一条覆盖 `f`..`l` 多笔撮合)
    /// - OKX `trades`：线路已归集 (`count` 为合并笔数；注意按 `source` 拆分，
    ///   同一主动单同一价位若对手方混有 ELP 与普通挂单会分成两条)
    /// - Hyperliquid `trades`：线路**逐笔**下发，由
    ///   `crate::exchange::hyperliquid::codec::aggregate_trades` 在单条消息内归集
    ///   (零额外延迟，不缓冲)
    ///
    /// 回测侧同样使用 Binance `aggTrades` 数据集
    /// (见 [`crate::backtest::BinanceDataKind::AggTrades`])，与实盘同粒度 —— 否则成交流
    /// 因子在回测里调好的参数上线即漂移。
    ///
    /// 口径由 `crate::exchange::trades_conformance` 的联网测试守住。
    Trades { symbol: Symbol },
    /// 标记价格
    MarkPrice { symbol: Symbol },
    /// 指数价格
    IndexPrice { symbol: Symbol },
    /// K线
    Candle { symbol: Symbol, interval: CandleInterval },
}

impl SubscriptionKind {
    /// 获取订阅的 symbol
    pub fn symbol(&self) -> &Symbol {
        match self {
            SubscriptionKind::FundingRate { symbol }
            | SubscriptionKind::BBO { symbol }
            | SubscriptionKind::Trades { symbol }
            | SubscriptionKind::MarkPrice { symbol }
            | SubscriptionKind::IndexPrice { symbol }
            | SubscriptionKind::Candle { symbol, .. } => symbol,
        }
    }
}

// ============================================================================
// ExchangeClient trait (仅 REST)
// ============================================================================

/// 交易所客户端统一接口
///
/// 仅封装交易所的 REST 交互，WebSocket Actor 由 ManagerActor 直接创建
///
/// # 数量口径契约
///
/// **返回值中的一切数量必须是币本位**（见 [`crate::domain::Quantity`]）。若交易所 REST 用
/// 合约张数计量（OKX 的 SWAP/FUTURES），折算是各实现自己的责任，不得把张数交给调用方 ——
/// 那样每个调用点都要记着乘一次，漏一处便静默算错 contract_size 倍。
///
/// 反方向（[`ExchangeClient::place_order`] 的入参）例外：订单在
/// `StrategyRunner::convert_order` 中已按 `SymbolMeta` 转为交易所下单单位。
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

    /// 撤单
    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError>;

    /// 查询当前挂单（live + partially_filled）
    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError>;

    /// 设置杠杆
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError>;

    /// 获取账户信息 (净值 + 总持仓名义价值)
    async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError>;

    /// 启动期查询所有 symbol 的持仓
    ///
    /// 用于在 executor 注册之后、市场订阅之前同步初始状态，避免策略基于陈旧 / 缺失的
    /// position 做出决策。**没有默认实现**——每个交易所都必须显式表态：
    /// - REST 直查（Binance）：实际请求账户持仓接口
    /// - 暂走私有 WS 推送 snapshot（OKX/HL/IBKR）：返回 `Ok(vec![])` 并在方法上注释
    ///   说明数据从哪里来，避免"沉默漏推"
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError>;
}

// ============================================================================
// 订阅/取消订阅消息
// ============================================================================

/// 订阅消息
#[derive(Debug, Clone)]
pub struct Subscribe {
    pub kind: SubscriptionKind,
}

/// 批量订阅消息
#[derive(Debug, Clone)]
pub struct SubscribeBatch {
    pub kinds: Vec<SubscriptionKind>,
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
    #[error("Server closed connection: {0}")]
    ServerClosed(String),
    #[error("Parse error: {0}")]
    ParseError(String),
}

impl From<String> for WsError {
    fn from(s: String) -> Self {
        WsError::ParseError(s)
    }
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
    /// 批量订阅
    async fn subscribe_batch(&self, kinds: Vec<SubscriptionKind>) -> Result<(), String>;
    /// 取消订阅
    async fn unsubscribe(&self, kind: SubscriptionKind) -> Result<(), String>;
}

/// 为 ActorRef<A> 实现 ExchangeActorOps 的宏
///
/// 要求 A 实现 `Message<Subscribe>`, `Message<SubscribeBatch>`, `Message<Unsubscribe>`
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
                async fn subscribe_batch(&self, kinds: Vec<$crate::exchange::SubscriptionKind>) -> Result<(), String> {
                    self.tell($crate::exchange::SubscribeBatch { kinds })
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

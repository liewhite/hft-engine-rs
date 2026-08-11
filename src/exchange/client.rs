//! 交易所客户端统一抽象
//!
//! ExchangeClient trait 封装交易所 REST 交互

use crate::domain::{
    AccountInfo, CandleInterval, Exchange, ExchangeError, Order, OrderId, OrderType, OrderUpdate,
    Position, Symbol, SymbolMeta,
};
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
// 交易所接入配置
// ============================================================================

/// 单个交易所的接入配置。
///
/// **"启用哪个所 + 计价币"与"有没有凭证"是两件事**，故拆开：
/// - `quote` / `dex` 决定接入哪个市场、内部符号如何拼成交易所符号 —— 公共行情也需要它们
/// - `credentials` 只决定能否接入**私有流与下单**；`None` = 只接公共行情
///
/// 二者原本合在凭证结构里，导致"跑模拟盘也必须先配 API key"。拆开后模拟盘可完全脱离凭证，
/// 且同一进程内可以让一部分所只读、另一部分所可交易。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExchangeAccess<C> {
    /// 计价币种 (e.g., "USDT", "USDC")
    pub quote: String,
    /// Perp DEX 名称：仅 Hyperliquid 有意义（"" = 默认 perp DEX，"xyz" = 股票永续），
    /// 其余交易所留空
    #[serde(default)]
    pub dex: String,
    /// 私有访问凭证。None = 只接公共行情，不下单、不接私有流
    #[serde(default = "Option::default")]
    pub credentials: Option<C>,
}

impl<C> ExchangeAccess<C> {
    /// 是否具备私有访问能力（下单 / 私有流）
    pub fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }
}

// ============================================================================
// 单位边界：ExchangeOrder
// ============================================================================

/// 已转换为**交易所下单单位**、并按交易所精度取整的订单。
///
/// # 为什么需要一个独立类型
///
/// 系统里存在两种数量单位（见 [`crate::domain::Quantity`]）：domain 的币本位、交易所的下单
/// 单位（OKX 的 SWAP 是张）。两者都是 `f64`，编译器分不开。此前出向折算的产物仍然是
/// `Order`，于是同一个 `Order.quantity` 在流水线前半段是币、后半段是张 —— 同一字段两种含义，
/// 靠读代码的人自己记住处在哪一段。这不是注释能解决的问题，因为**错了不会报错，只会静默
/// 算错 contract_size 倍**。
///
/// 用独立类型把这条边界钉在类型系统里：
/// - domain 里流转的一律是 `Order`（币本位），策略、`StrategyRunner`、`SimState`、回测同理
/// - 只有 [`ExchangeOrder`] 携带交易所单位，且它**只能**由 [`ExchangeOrder::from_domain`] 产生
/// - [`ExchangeClient::place_order`] 只收 `ExchangeOrder`，因此不可能把币本位数量直发上线
///
/// 与入向的做法对称：入向由 codec 的签名要求 `&SymbolMeta` 来强制折算，出向由本类型的
/// 构造函数要求 `&SymbolMeta` 来强制折算。两个方向的单位知识都收在本文件。
#[derive(Debug, Clone)]
pub struct ExchangeOrder(Order);

impl ExchangeOrder {
    /// 币本位 -> 交易所下单单位，按交易所精度取整价格与数量，并校验数量下界。
    ///
    /// 取整方向**有意不同**（沿用改造前的行为，此处只是搬家）：
    /// - 数量 `round_size_down`：**向下**取整，宁可少下一点，也不超出策略意图的数量
    /// - 价格 `round_price`：取**最近**的合法 tick（`StepFormatter` 是四舍五入），因为价格
    ///   向下取整对买单是让价、对卖单是抢价，方向语义不一致，反而更难推理
    ///
    /// # 数量下界校验
    ///
    /// 折算与下界校验统一走 [`SymbolMeta::checked_exchange_qty`]（模拟柜台同一出处，
    /// 保证实盘必拒的单模拟盘也拒）。取整为 0 / 低于 `min_order_size` 都在本地拦下并
    /// 返回可定位的原因，由调用方反馈为 `OrderUpdate(Error)` —— 策略能看到失败，
    /// 而不是每个事件白打一次 REST。
    pub fn from_domain(order: Order, meta: &SymbolMeta) -> Result<Self, String> {
        let quantity = meta.checked_exchange_qty(order.quantity)?;
        let order_type = match &order.order_type {
            OrderType::Market => OrderType::Market,
            OrderType::Limit { price, tif } => OrderType::Limit {
                price: meta.round_price(*price),
                tif: *tif,
            },
        };
        Ok(Self(Order {
            order_type,
            quantity,
            ..order
        }))
    }

    /// 交易所单位的订单内容（供各 client 组装请求）。
    pub fn inner(&self) -> &Order {
        &self.0
    }
}

// ============================================================================
// ExchangeClient trait (仅 REST)
// ============================================================================

/// 交易所客户端统一接口
///
/// 仅封装交易所的 REST 交互，WebSocket Actor 由 ManagerActor 直接创建
///
/// # 数量口径契约（双向对称）
///
/// - **出向**：[`ExchangeClient::place_order`] 只接受 [`ExchangeOrder`] —— 已折算为交易所
///   下单单位、已取整。类型系统保证币本位数量不可能被直发上线。
/// - **入向**：返回值中的一切数量**必须是币本位**（见 [`crate::domain::Quantity`]）。若交易所
///   REST 用张数计量（OKX 的 SWAP/FUTURES），折算是各实现自己的责任，不得把张数交给调用方
///   —— 那样每个调用点都要记着乘一次，漏一处便静默算错 contract_size 倍。
#[async_trait]
pub trait ExchangeClient: Send + Sync + 'static {
    /// 获取交易所标识
    fn exchange(&self) -> Exchange;

    /// 获取所有交易对元数据
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError>;

    /// 获取指定交易对元数据
    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError>;

    /// 下单。入参已是交易所单位，见 [`ExchangeOrder`]。
    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError>;

    /// 撤单
    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError>;

    /// 查询当前挂单（live + partially_filled）
    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError>;

    /// 获取账户信息 (净值 + 总持仓名义价值)
    async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError>;

    /// 查询账户当前持仓（**必须真实请求交易所**，不得返回空占位）
    ///
    /// 本方法服务于持仓维护模型的**两条通道**，两者都不容许假数据：
    /// - **基线**：`ManagerActor` 投产期调用一次，产出
    ///   [`crate::messaging::PositionBaseline`]（投产握手载荷）。基线只有这一次机会——
    ///   之后持仓全程由 `Fill` 累加，没有第二个来源能纠正它，故拉取失败即拒绝启动。
    /// - **对账**：`PositionReconcileActor` 周期性调用，作为读数与「基线 + Fill」的结果比对。
    ///
    /// 因此返回 `Ok(vec![])` 只有一个合法含义：**该账户确实没有任何持仓**（含"未配置凭证、
    /// 只接公共行情"的情形）。用空 Vec 表示"数据在别处（私有 WS）"会让基线静默变成全零、
    /// 且对账把真实持仓判成漂移。
    ///
    /// 数量口径为币本位，见 trait 级文档。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Side, TimeInForce};
    use crate::exchange::utils::StepFormatter;
    use std::sync::Arc;

    /// OKX BTC-USDT-SWAP：每张 0.01 币、价格步长 0.1、数量步长 1 张
    fn okx_btc_meta() -> SymbolMeta {
        SymbolMeta {
            exchange: Exchange::OKX,
            symbol: "BTC".to_string(),
            price_formatter: Arc::new(StepFormatter::new(0.1)),
            size_step: 1.0,
            min_order_size: 1.0,
            contract_size: 0.01,
        }
    }

    fn coin_order(quantity: f64, price: f64) -> Order {
        Order {
            id: String::new(),
            exchange: Exchange::OKX,
            symbol: "BTC".to_string(),
            side: Side::Long,
            order_type: OrderType::Limit {
                price,
                tif: TimeInForce::PostOnly,
            },
            quantity,
            reduce_only: false,
            client_order_id: "c1".to_string(),
        }
    }

    #[test]
    fn coin_quantity_is_converted_to_contracts() {
        let order = coin_order(0.12, 62_500.0);
        let wire = ExchangeOrder::from_domain(order, &okx_btc_meta()).expect("合法订单");
        // 0.12 币 / 0.01 = 12 张
        assert!((wire.inner().quantity - 12.0).abs() < 1e-9);
    }

    #[test]
    fn quantity_rounds_down_to_size_step() {
        // 0.125 币 = 12.5 张 -> 向下取整 12 张（宁可少下，不超出策略意图）
        let wire = ExchangeOrder::from_domain(coin_order(0.125, 62_500.0), &okx_btc_meta()).expect("合法订单");
        assert!((wire.inner().quantity - 12.0).abs() < 1e-9);
    }

    #[test]
    fn price_is_rounded_to_exchange_tick() {
        let wire = ExchangeOrder::from_domain(coin_order(0.12, 62_500.17), &okx_btc_meta()).expect("合法订单");
        match wire.inner().order_type {
            // 价格取**最近** tick（62500.17 -> 62500.2），与数量的向下取整不同
            OrderType::Limit { price, .. } => assert!((price - 62_500.2).abs() < 1e-9),
            OrderType::Market => panic!("expected limit"),
        }
    }

    /// contract_size = 1 的所（Binance/HL/IBKR）折算是恒等变换，只剩取整
    #[test]
    fn unit_contract_size_leaves_quantity_unchanged() {
        let meta = SymbolMeta {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            price_formatter: Arc::new(StepFormatter::new(0.1)),
            size_step: 0.001,
            min_order_size: 0.001,
            contract_size: 1.0,
        };
        let wire = ExchangeOrder::from_domain(coin_order(0.1234, 62_500.0), &meta).expect("合法订单");
        assert!((wire.inner().quantity - 0.123).abs() < 1e-9);
    }

    /// 不足一个 size_step 的量取整为 0，必须本地拒绝 —— 照发必被交易所拒，
    /// 且策略会陷入"下单 → 被拒 → 重下"的静默重试环
    #[test]
    fn quantity_rounding_to_zero_is_rejected() {
        // 0.004 币 = 0.4 张 -> 向下取整 0 张
        let err = ExchangeOrder::from_domain(coin_order(0.004, 62_500.0), &okx_btc_meta())
            .expect_err("取整为 0 的订单必须被拒");
        assert!(err.contains("取整后为 0"), "got: {err}");
    }

    /// 低于交易所最小下单量同样本地拒绝（min_order_size 为交易所单位）
    #[test]
    fn quantity_below_min_order_size_is_rejected() {
        let mut meta = okx_btc_meta();
        meta.min_order_size = 5.0; // 最小 5 张
        // 0.03 币 = 3 张 < 5 张
        let err = ExchangeOrder::from_domain(coin_order(0.03, 62_500.0), &meta)
            .expect_err("低于最小下单量的订单必须被拒");
        assert!(err.contains("最小下单量"), "got: {err}");
    }

    /// 其余字段原样透传（方向/reduce_only/client_order_id 不属于单位换算范畴）
    #[test]
    fn non_quantity_fields_pass_through() {
        let mut order = coin_order(0.12, 62_500.0);
        order.reduce_only = true;
        order.side = Side::Short;
        let wire = ExchangeOrder::from_domain(order, &okx_btc_meta()).expect("合法订单");
        assert!(wire.inner().reduce_only);
        assert_eq!(wire.inner().side, Side::Short);
        assert_eq!(wire.inner().client_order_id, "c1");
    }
}

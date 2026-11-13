//! 交易所客户端统一抽象
//!
//! ExchangeClient trait 封装交易所 REST 交互

use crate::domain::{
    AccountInfo, CandleInterval, Exchange, ExchangeError, Order, OrderId, PlaceError, Position,
    Symbol, SymbolMeta,
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
    Candle {
        symbol: Symbol,
        interval: CandleInterval,
    },
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

/// 账户类数据流：适配层用 REST 轮询出来、发到账户总线的账户读数。与公共订阅不同，它们按
/// 账户收费（占限速）、取不到时还会停机（陈旧守卫），所以不默认起：策略经
/// [`crate::strategy::Strategy::account_streams`] 声明需要，投产时才在该所起（#757）。
///
/// 加一种：这里加一个变体，在提供它的所的装配函数（`engine::live::assembly`）里登记怎么起。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountStream {
    /// 账户期权组合的希腊值（`ExchangeAccountData::Greeks`；目前只有 OKX 提供）
    Greeks,
}

// ============================================================================
// 交易所接入配置
// ============================================================================

/// 单个交易所的接入配置。
///
/// **"启用哪个所 + 计价币"与"有没有凭证"是两件事**，故拆开：
/// - `quote` 决定接入哪个市场、内部符号如何拼成交易所符号 —— 公共行情也需要它们
/// - `credentials` 只决定能否接入**私有流与下单**；`None` = 只接公共行情
///
/// 二者原本合在凭证结构里，导致"跑模拟盘也必须先配 API key"。拆开后模拟盘可完全脱离凭证，
/// 且同一进程内可以让一部分所只读、另一部分所可交易。
///
/// 不认识的键报错（`deny_unknown_fields`）：旧配置写的 `"dex": "xyz"` 若被静默忽略，
/// 进程会接到主站而不是股票市场。Hyperliquid 有自己的接入配置（带 `dexes`），见
/// `crate::exchange::hyperliquid::HyperliquidAccess`。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeAccess<C> {
    /// 计价币种 (e.g., "USDT", "USDC")
    pub quote: String,
    /// 私有访问凭证。None = 只接公共行情，不下单、不接私有流
    #[serde(default = "Option::default")]
    pub credentials: Option<C>,
}

// ============================================================================
// 单位边界：ExchangeOrder
// ============================================================================

/// 已转换为**交易所下单单位**的订单（价格与数量的精度取整在 `StrategyRunner` 已做过）。
///
/// # 为什么需要一个独立类型
///
/// 系统里存在两种数量单位（见 [`crate::domain::Quantity`]）：domain 的币本位、交易所的下单
/// 单位（OKX 的 SWAP 是张）。两者都是 `Quantity`，编译器分不开。此前出向折算的产物仍然是
/// `Order`，于是同一个 `Order.quantity` 在流水线前半段是币、后半段是张 —— 同一字段两种含义，
/// 靠读代码的人自己记住处在哪一段。这不是注释能解决的问题，因为**错了不会报错，只会静默
/// 算错 contract_size 倍**。
///
/// 用独立类型把这条边界钉在类型系统里：
/// - domain 里流转的一律是 `Order`（币本位），策略、`StrategyRunner`、`SimState`、回测同理
/// - 只有 [`ExchangeOrder`] 携带交易所单位，且它**只能**由 [`ExchangeOrder::from_domain`] 产生
/// - [`AccountClient::place_order`] 只收 `ExchangeOrder`，因此不可能把币本位数量直发上线
///
/// 与入向的做法对称：入向由 codec 的签名要求 `&SymbolMeta` 来强制折算，出向由本类型的
/// 构造函数要求 `&SymbolMeta` 来强制折算。两个方向的单位知识都收在本文件。
#[derive(Debug, Clone)]
pub struct ExchangeOrder(Order);

impl ExchangeOrder {
    /// 币本位 -> 交易所下单单位，并校验数量下界。
    ///
    /// 只换单位、不取整：价格与数量的精度取整只在下单闸门（`StrategyRunner::admit`）做一次，
    /// 回测、模拟盘、实盘发出的是同一张单。换单位与下界校验走 [`SymbolMeta::checked_exchange_qty`]
    /// （模拟柜台同一出处，保证实盘必拒的单模拟盘也拒）；取整为 0 / 低于 `min_order_size` 在本地拦下，
    /// 返回可定位的原因，由调用方回报为「被拒」—— 策略能看到失败，而不是每个事件白打一次 REST。
    pub fn from_domain(order: Order, meta: &SymbolMeta) -> Result<Self, String> {
        let quantity = meta.checked_exchange_qty(order.quantity)?;
        Ok(Self(Order { quantity, ..order }))
    }

    /// 交易所单位的订单内容（供各 client 组装请求）。
    pub fn inner(&self) -> &Order {
        &self.0
    }
}

// ============================================================================
// REST 接口：公共 / 账户两个 trait
// ============================================================================

/// 账户上的一张挂单，只为启动清场（见 [`AccountClient::fetch_open_orders`]）：交易所的合约名与订单号
/// 原样保留、不映射成内部 symbol —— 要撤的包括本引擎不接入的品种，撤单由适配层按它们自己撤。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrder {
    /// 交易所自己的合约名（如 `BTCUSDT`、`BTC-USDT-SWAP`、`xyz:TSLA`、IBKR conid）
    pub instrument: String,
    /// 交易所订单号
    pub order_id: OrderId,
}

/// **公共**数据接口：不需要凭证，任何接入的交易所都提供。
///
/// 仅封装 REST 交互，WebSocket Actor 由各适配层的 `setup_*` 创建。
///
/// # 为什么与 [`AccountClient`] 分开
///
/// "这个所有没有账户"是**能力**问题，必须由类型表达，不能由返回值表达
/// （见 `docs/architecture.md` 原则 P2）。合成一个 trait 时，无凭证的所只能让
/// `fetch_positions` 返回 `Ok(vec![])` —— 调用方从此分不清"真的空仓"与"根本不该问"，
/// 而"有没有账户"这一事实又要靠另一个 `bool` 字段另存一份。
///
/// 拆开之后：有没有账户 = `ExchangeSetup.account` 是不是 `None`，一处表达，编译期可查。
///
/// # 入向数量口径
///
/// 返回值中的一切数量**必须是币本位**（见 [`crate::domain::Quantity`]）。若交易所
/// REST 用张数计量（OKX 的 SWAP/FUTURES），折算是各实现自己的责任，不得把张数交给调用方
/// —— 那样每个调用点都要记着乘一次，漏一处便静默算错 contract_size 倍。
#[async_trait]
pub trait ExchangeClient: Send + Sync + 'static {
    /// 获取交易所标识
    fn exchange(&self) -> Exchange;

    /// 获取所有交易对元数据
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError>;
}

/// **账户私有**接口：只有配了凭证的所才有这个对象。
///
/// 持有一个 `Arc<dyn AccountClient>` 即意味着"该所有账户可用"—— 这是能力的**唯一**表达。
/// 各所的实现是单独的账户类型（`BinanceAccount` / `OkxAccount` / `HyperliquidAccount`），凭证是
/// 它的必填字段，只在配了凭证时由 `setup_*` 构造（#813）；公共接口（[`ExchangeClient`]）是另一个
/// 不带凭证的类型。因此本 trait 的任何方法都没有「没有凭证」这条路。
///
/// # 出向数量口径
///
/// [`AccountClient::place_order`] 只接受 [`ExchangeOrder`] —— 已折算为交易所下单单位、
/// 已取整。类型系统保证币本位数量不可能被直发上线。入向口径同 [`ExchangeClient`]。
#[async_trait]
pub trait AccountClient: Send + Sync + 'static {
    /// 获取交易所标识
    fn exchange(&self) -> Exchange;

    /// 下单。入参已是交易所单位，见 [`ExchangeOrder`]。
    ///
    /// 失败时必须如实分成「被拒绝」与「结果不确定」两种，见 [`PlaceError`]：只有交易所
    /// 明确拒单或请求根本没发出才算被拒绝，其余（超时、5xx、断线、回复解析不了……）都是
    /// 结果不确定。拿不准的一律报不确定 —— 报错了的代价是停机，报成拒绝的代价是重复下单。
    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError>;

    /// 按**客户端订单号**撤单（本引擎下单时分配的 `client_order_id`）：下单请求还没结论、
    /// 还不知道交易所订单号的单也能撤。返回 `Ok` 只表示交易所受理了撤单，终态以订单回报为准。
    async fn cancel_order(
        &self,
        symbol: &Symbol,
        client_order_id: &str,
    ) -> Result<(), ExchangeError>;

    /// 账户上**全部**挂单：本适配层交易的产品线（币安 U 本位合约、OKX 全部品种、HL 全部 perp dex、
    /// IBKR 本账户）上的每一张，不限本引擎接入的币、不管是谁下的。账户由引擎独占
    /// （`docs/architecture.md`「账户独占」），启动时据此撤净（见 [`Self::cancel_open_order`]）。
    ///
    /// 必须完整：漏一张，清场就会谎报干净。翻页没取完、字段解析不了都报错，不返回半份。
    async fn fetch_open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError>;

    /// 撤 [`Self::fetch_open_orders`] 列出的一张挂单（启动清场用）。返回 `Ok` 只表示交易所受理了撤单；
    /// 撤净与否由调用方再查一次挂单确认。
    async fn cancel_open_order(&self, order: &OpenOrder) -> Result<(), ExchangeError>;

    /// 获取账户信息 (净值 + 总持仓名义价值)
    async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError>;

    /// 查询账户当前持仓（**必须真实请求交易所**，不得返回空占位）
    ///
    /// 本方法服务于持仓维护模型的**两条通道**，两者都不容许假数据：
    /// - **基线**：manager 投产期调用一次，产出
    ///   [`crate::messaging::PositionBaseline`]（投产握手载荷）。基线只有这一次机会——
    ///   之后持仓全程由 `Fill` 累加，没有第二个来源能纠正它，故拉取失败即拒绝启动。
    /// - **对账**：`PositionLedger` 周期性调用，作为读数与「基线 + Fill」的结果比对。
    ///
    /// 因此返回 `Ok(vec![])` 只有一个合法含义：**该账户确实没有任何持仓**。
    /// "未配置凭证"这一情形已由类型排除 —— 那时根本不存在本对象。
    ///
    /// 只含本引擎接入的合约（映射不到内部 symbol 的别的品种不列入），每个 symbol 至多一条；
    /// 消费方用 [`index_positions`] 建索引，重复即报错。数量口径为币本位，见 trait 级文档。
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError>;
}

/// 交易所 REST 的 HTTP client（各所的 REST client、Binance listenKey、OKX 历史 K 线都用它）：
/// 单次请求 10s 超时，其余共用设置（空闲连接、UA）见 [`crate::net`]。
pub(crate) fn exchange_http_client(exchange: Exchange) -> Result<reqwest::Client, ExchangeError> {
    crate::net::http(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| ExchangeError::transport(exchange, e.to_string()))
}

/// 下单 HTTP 请求在拿到回复之前就失败了：连接都没建立（`is_connect`）说明请求没送到交易所，
/// 确定没挂上；其余（超时、连接中途断开、读回复失败）请求可能已到交易所，结果不确定。
pub(crate) fn place_send_error(exchange: Exchange, e: reqwest::Error) -> PlaceError {
    if e.is_connect() {
        PlaceError::unsent(format!("{exchange} connect failed: {e}"))
    } else {
        PlaceError::indeterminate(format!("{exchange} request failed: {e}"))
    }
}

/// 把一所的持仓快照按 symbol 建索引。同一 symbol 出现两条说明映射把两个合约混成了一个
/// （如 BTC-USDT 与 BTC-USDC），静默覆盖会记错持仓 —— 报错。
pub fn index_positions(
    positions: Vec<Position>,
) -> Result<std::collections::HashMap<Symbol, Position>, String> {
    let mut map = std::collections::HashMap::with_capacity(positions.len());
    for p in positions {
        if let Some(prev) = map.insert(p.symbol.clone(), p) {
            return Err(format!(
                "{} 持仓快照里 {} 出现两条（品种映射把不同合约混成了同一个 symbol）",
                prev.exchange, prev.symbol
            ));
        }
    }
    Ok(map)
}

/// **已结算公共资金费率**的历史拉取（无需凭证）。资金费套利按「过去 24h 实际结算累加」比较
/// 各所，每接入一所就实现一次；调用方拿 `Vec<Arc<dyn SettledFundingSource>>` 统一轮询，
/// 加一所不改调用方。
#[async_trait]
pub trait SettledFundingSource: Send + Sync + 'static {
    fn exchange(&self) -> Exchange;

    /// 自 `start_ms`（含）起已结算的费率，按结算时间升序。该所没有这个币时报错。
    async fn fetch_settled_funding(
        &self,
        symbol: &Symbol,
        start_ms: crate::domain::Timestamp,
    ) -> Result<Vec<crate::domain::SettledFunding>, ExchangeError>;
}

/// **本账户**资金费收付流水的拉取（需要凭证；与 [`SettledFundingSource`] 的公共费率不同）。
/// 由通用的 `FundingFeePoller` 定时调用并发布到账户总线，所别只在各所的实现里；
/// 加一所只实现本 trait、在该所节点里起一个轮询器。
#[async_trait]
pub trait FundingFeeSource: Send + Sync + 'static {
    fn exchange(&self) -> Exchange;

    /// `[start_ms, end_ms]` 区间内本账户的资金费结算明细。每条的 `tran_id` 在该所内稳定唯一，
    /// 调用方据此去重（同一笔可能在相邻两次拉取里都出现）。
    async fn fetch_funding_fees(
        &self,
        start_ms: crate::domain::Timestamp,
        end_ms: crate::domain::Timestamp,
    ) -> Result<Vec<crate::domain::FundingFee>, ExchangeError>;
}

// ============================================================================
// 交易所节点的命令与句柄
// ============================================================================

/// 发给交易所节点的命令（各所节点的收件箱消息类型）
#[derive(Debug, Clone)]
pub enum ExchangeCmd {
    /// 订阅一批行情（只在投产时发一次；不退订）
    SubscribeBatch(Vec<SubscriptionKind>),
}

/// 一个交易所节点的句柄：引擎经它订阅行情。各所装配函数（`engine::live::assembly`）
/// 起好节点后交出它；节点的生死归任务树管，句柄只是地址。
#[derive(Debug, Clone)]
pub struct ExchangeHandle(crate::tasks::Addr<ExchangeCmd>);

impl ExchangeHandle {
    pub fn new(addr: crate::tasks::Addr<ExchangeCmd>) -> Self {
        Self(addr)
    }

    pub fn subscribe_batch(&self, kinds: Vec<SubscriptionKind>) -> Result<(), crate::tasks::Gone> {
        self.0.send(ExchangeCmd::SubscribeBatch(kinds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    /// 快照里同一 symbol 两条（映射把两个合约混成了一个）必须报错，不能后一条覆盖前一条
    #[test]
    fn duplicate_symbol_in_position_snapshot_is_an_error() {
        let p = |symbol: &str, size: Decimal| Position {
            exchange: Exchange::OKX,
            symbol: symbol.into(),
            size,
        };
        let ok = index_positions(vec![p("BTC", dec!(1.0)), p("ETH", dec!(-2.0))]).unwrap();
        assert_eq!(ok["BTC"].size, dec!(1.0));
        assert!(index_positions(vec![p("BTC", dec!(1.0)), p("BTC", dec!(5.0))]).is_err());
    }
    use crate::domain::PriceStep;
    use crate::domain::{OrderType, Side, TimeInForce};
    use std::sync::Arc;

    /// OKX BTC-USDT-SWAP：每张 0.01 币、价格步长 0.1、数量步长 1 张
    fn okx_btc_meta() -> SymbolMeta {
        SymbolMeta {
            exchange: Exchange::OKX,
            symbol: "BTC".to_string(),
            price_rule: Arc::new(PriceStep::new(dec!(0.1))),
            size_step: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
            min_order_size: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
            min_notional: 0.0,
            contract_size: crate::domain::PositiveDecimal::new(dec!(0.01)).unwrap(),
        }
    }

    fn coin_order(quantity: Decimal, price: Decimal) -> Order {
        Order {
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
        let order = coin_order(dec!(0.12), dec!(62_500.0));
        let wire = ExchangeOrder::from_domain(order, &okx_btc_meta()).expect("合法订单");
        // 0.12 币 / 0.01 = 12 张
        assert_eq!(wire.inner().quantity, dec!(12.0));
    }

    #[test]
    fn quantity_rounds_down_to_size_step() {
        // 0.125 币 = 12.5 张 -> 向下取整 12 张（宁可少下，不超出策略意图）
        let wire =
            ExchangeOrder::from_domain(coin_order(dec!(0.125), dec!(62_500.0)), &okx_btc_meta())
                .expect("合法订单");
        assert_eq!(wire.inner().quantity, dec!(12.0));
    }

    /// 出口只换单位：价格原样发出（精度取整只在下单闸门做一次，#812）
    #[test]
    fn price_passes_through_unchanged() {
        let wire =
            ExchangeOrder::from_domain(coin_order(dec!(0.12), dec!(62_500.2)), &okx_btc_meta())
                .expect("合法订单");
        match wire.inner().order_type {
            OrderType::Limit { price, .. } => assert_eq!(price, dec!(62500.2)),
            OrderType::Market => panic!("expected limit"),
        }
    }

    /// contract_size = 1 的所（Binance/HL/IBKR）折算是恒等变换，只剩取整
    #[test]
    fn unit_contract_size_leaves_quantity_unchanged() {
        let meta = SymbolMeta {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            price_rule: Arc::new(PriceStep::new(dec!(0.1))),
            size_step: crate::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
            min_order_size: crate::domain::PositiveDecimal::new(dec!(0.001)).unwrap(),
            min_notional: 0.0,
            contract_size: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
        };
        let wire = ExchangeOrder::from_domain(coin_order(dec!(0.1234), dec!(62_500.0)), &meta)
            .expect("合法订单");
        assert_eq!(wire.inner().quantity, dec!(0.123));
    }

    /// 不足一个 size_step 的量取整为 0，必须本地拒绝 —— 照发必被交易所拒，
    /// 且策略会陷入"下单 → 被拒 → 重下"的静默重试环
    #[test]
    fn quantity_rounding_to_zero_is_rejected() {
        // 0.004 币 = 0.4 张 -> 向下取整 0 张
        let err =
            ExchangeOrder::from_domain(coin_order(dec!(0.004), dec!(62_500.0)), &okx_btc_meta())
                .expect_err("取整为 0 的订单必须被拒");
        assert!(err.contains("取整后为 0"), "got: {err}");
    }

    /// 低于交易所最小下单量同样本地拒绝（min_order_size 为交易所单位）
    #[test]
    fn quantity_below_min_order_size_is_rejected() {
        let mut meta = okx_btc_meta();
        meta.min_order_size = crate::domain::PositiveDecimal::new(dec!(5.0)).unwrap(); // 最小 5 张
                                                                                       // 0.03 币 = 3 张 < 5 张
        let err = ExchangeOrder::from_domain(coin_order(dec!(0.03), dec!(62_500.0)), &meta)
            .expect_err("低于最小下单量的订单必须被拒");
        assert!(err.contains("最小下单量"), "got: {err}");
    }

    /// 上游算出负数量时本地拒绝（方向在 Side 里，负数是上游算错）
    #[test]
    fn negative_quantity_is_rejected() {
        let err =
            ExchangeOrder::from_domain(coin_order(dec!(-0.12), dec!(62_500.0)), &okx_btc_meta())
                .expect_err("负数量必须被拒");
        assert!(err.contains("为负"), "got: {err}");
    }

    /// 其余字段原样透传（方向/reduce_only/client_order_id 不属于单位换算范畴）
    #[test]
    fn non_quantity_fields_pass_through() {
        let mut order = coin_order(dec!(0.12), dec!(62_500.0));
        order.reduce_only = true;
        order.side = Side::Short;
        let wire = ExchangeOrder::from_domain(order, &okx_btc_meta()).expect("合法订单");
        assert!(wire.inner().reduce_only);
        assert_eq!(wire.inner().side, Side::Short);
        assert_eq!(wire.inner().client_order_id, "c1");
    }
}

//! 事件模型：分类是**结构**，不是推断。
//!
//! 两个封闭枚举把"公共行情"与"账户私有"在类型上分开：
//! - [`MarketEvent`]（[`MarketData`]）：无账户归属，一份服务所有账户，走 MarketBus；
//! - [`AccountEvent`]（[`AccountData`]）：账户是**必填结构字段**，走 AccountBus ——
//!   实盘适配层与本地柜台发布同一个类型，只是 `account` 标签不同。
//!
//! 历史上两类混在一个平铺枚举里、账户归属靠"来源即 Live"的推断 + 一张手工分类表
//! （`is_account_private`）维护 —— 新增私有变体漏改一行的失效方向是"实盘私有事件广播给
//! 模拟策略"（危险侧、编译器不报）。拆开后账户隔离由类型系统保证，分类表消失。
//!
//! [`IncomeEvent`] 是策略与状态层的统一视图（两变体 enum）：策略的 `on_event` 仍然只有
//! 一个入口，match 变为两层。

use crate::domain::{to_f64, Price, Quantity};
use crate::domain::{
    AccountId, Balance, BorrowFee, Candle, Exchange, ExchangeRate, Fill, FundingFee, FundingRate,
    Greeks, IndexPrice, MarkPrice, MarketStatus, MarketTrade, OrderUpdate, Symbol, Timestamp, BBO,
};
use std::any::Any;
use std::sync::Arc;

/// 用户域自定义事件：框架只负责运送与路由，不理解其内容。
///
/// 框架事件枚举（[`MarketData`] 其余变体）是**封闭**的 —— 引擎必须穷尽处理它们；
/// 本类型是枚举上唯一的**开放**扩展点：新事件类型只需定义一个 payload struct，
/// 不改框架任何代码。
///
/// # 只有一个方向：外部向策略
///
/// 经 `ManagerHandle::publish_custom_event` 注入行情总线，按 scope 路由（见下）后进入订阅者的
/// `Strategy::on_event`。策略向外发自定义事件的出口（`Signal::Emit`）没有用户，已删（#814）；
/// 策略要对外报读数用 `Strategy::gauges`。
///
/// # 路由与账户归属
///
/// - 带 `(exchange, symbol)` scope 的事件按既有订阅关系定向投递，无 scope 的广播 ——
///   复用 [`IncomeEvent::delivery`] 的推导，无第二套路由。
/// - 自定义事件**没有账户归属**（如同行情，故属 [`MarketData`]）：实盘与模拟账户的
///   策略都会收到。需要区分时在 payload 里自带判别字段。
///
/// # 消费方式
///
/// `event.get::<T>()` 按具体类型取回，类型不符返回 `None` —— 订阅者只对自己认识的
/// 类型起反应，这也是分发的实际粒度。`name` 由 payload 类型名自动填充，供日志定位。
///
/// # 两个静默失败模式（payload 作者须知）
///
/// - **跨版本 TypeId**：`get::<T>()` 按 `TypeId` 判别。工作区若同时链入某 payload crate
///   的两个版本，两边的 `T` 是不同类型，`get` 静默得 `None` —— 表现为"订阅者收到但
///   不认识"。payload 类型应定义在单一 crate 的单一版本里。
/// - **payload 应为不可变数据**：同一份 payload 经 `Arc` 在所有订阅者间共享。若其中
///   藏了内部可变性（`Mutex`/`RefCell` 等），就变成了订阅者之间的共享可变状态 ——
///   这会绕开"事件是值"的模型，禁止。
#[derive(Clone)]
pub struct CustomEvent {
    /// 定向路由的 scope。单字段保证"要么完整、要么没有"——不存在只有 exchange 或
    /// 只有 symbol 的半 scope 状态（那会静默降级为广播，与构造意图相反）。
    scope: Option<(Exchange, Symbol)>,
    /// payload 的类型名（自动填充，观测用；类型判别请用 [`Self::get`]，不要比对本字段）
    pub name: &'static str,
    /// 类型擦除的事件内容；`Arc` 使总线广播的 clone 零拷贝
    payload: Arc<dyn Any + Send + Sync>,
}

impl CustomEvent {
    /// 无 scope 的自定义事件：广播给所有策略 executor 与总线订阅者
    pub fn new<T: Any + Send + Sync>(payload: T) -> Self {
        Self {
            scope: None,
            name: std::any::type_name::<T>(),
            payload: Arc::new(payload),
        }
    }

    /// 带 `(exchange, symbol)` scope 的自定义事件：只投递给订阅了该 symbol 的 executor
    pub fn for_symbol<T: Any + Send + Sync>(
        exchange: Exchange,
        symbol: Symbol,
        payload: T,
    ) -> Self {
        Self {
            scope: Some((exchange, symbol)),
            name: std::any::type_name::<T>(),
            payload: Arc::new(payload),
        }
    }

    /// 定向路由的 scope；`None` 即广播
    pub fn scope(&self) -> Option<&(Exchange, Symbol)> {
        self.scope.as_ref()
    }

    /// 按具体类型取回 payload；类型不符返回 `None`
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.payload.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for CustomEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomEvent")
            .field("name", &self.name)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

// ============================================================================
// 公共市场事件
// ============================================================================

/// 公共市场事件：**无账户归属**，一份数据服务所有账户。
///
/// - `exchange_ts`: 交易所推送的时间戳
/// - `local_ts`: 本地接收时间戳
#[derive(Debug, Clone)]
pub struct MarketEvent {
    pub exchange_ts: Timestamp,
    pub local_ts: Timestamp,
    pub data: MarketData,
}

/// 公共市场数据。像 [`AccountData`] 一样，定位精度在**类型**上分档：币级（带 (所, symbol)）、
/// 所级（只带所）、全局（时钟）；自定义事件的范围由构造方给出。
///
/// 分档而不是一张平铺枚举：状态层按档分派时没有默认分支，币级状态（`SymbolState`）收不到
/// 所级或全局事件；新增一种行情先选档、再在该档的穷举 match 里写处理，漏了编译不过。
#[derive(Debug, Clone)]
pub enum MarketData {
    /// 币级：属于某个 (所, symbol)
    Symbol(SymbolMarketData),
    /// 所级：属于某个所
    Exchange(ExchangeMarketData),
    /// 时钟（全局，用于超时检测等定时任务）
    Clock,
    /// 用户域自定义事件（唯一的开放扩展点，见 [`CustomEvent`]）。
    /// 框架的状态层（`StateManager`/`SymbolState`）对它一律 no-op —— 只运送，不理解。
    Custom(CustomEvent),
}

/// 币级行情（封闭枚举）
#[derive(Debug, Clone)]
pub enum SymbolMarketData {
    FundingRate(FundingRate),
    BBO(BBO),
    /// 公共成交印记 (市场匿名成交)；回测/模拟盘撮合的成交来源
    MarketTrade(MarketTrade),
    MarkPrice(MarkPrice),
    IndexPrice(IndexPrice),
    /// K线实时推送
    Candle(Candle),
    /// 历史K线批量数据（订阅时一次性推送）
    HistoryCandles(HistoryCandles),
    /// 融券券源读数 (借券费 + 可借量)；参考数据，策略在 on_event 里消费
    BorrowFee(BorrowFee),
}

/// 所级行情（封闭枚举）
#[derive(Debug, Clone)]
pub enum ExchangeMarketData {
    /// 交易所市场状态变更
    ExchangeStatus {
        exchange: Exchange,
        status: MarketStatus,
    },
    /// 汇率读数 (货币对)；参考数据，策略在 on_event 里消费
    ExchangeRate(ExchangeRate),
}

impl SymbolMarketData {
    pub fn exchange(&self) -> Exchange {
        match self {
            SymbolMarketData::FundingRate(r) => r.exchange,
            SymbolMarketData::BBO(b) => b.exchange,
            SymbolMarketData::MarketTrade(t) => t.exchange,
            SymbolMarketData::MarkPrice(mp) => mp.exchange,
            SymbolMarketData::IndexPrice(ip) => ip.exchange,
            SymbolMarketData::Candle(c) => c.exchange,
            SymbolMarketData::HistoryCandles(h) => h.exchange,
            SymbolMarketData::BorrowFee(bf) => bf.exchange,
        }
    }

    pub fn symbol(&self) -> &Symbol {
        match self {
            SymbolMarketData::FundingRate(r) => &r.symbol,
            SymbolMarketData::BBO(b) => &b.symbol,
            SymbolMarketData::MarketTrade(t) => &t.symbol,
            SymbolMarketData::MarkPrice(mp) => &mp.symbol,
            SymbolMarketData::IndexPrice(ip) => &ip.symbol,
            SymbolMarketData::Candle(c) => &c.symbol,
            SymbolMarketData::HistoryCandles(h) => &h.symbol,
            SymbolMarketData::BorrowFee(bf) => &bf.symbol,
        }
    }
}

impl ExchangeMarketData {
    pub fn exchange(&self) -> Exchange {
        match self {
            ExchangeMarketData::ExchangeStatus { exchange, .. } => *exchange,
            ExchangeMarketData::ExchangeRate(er) => er.exchange,
        }
    }
}

impl MarketData {
    /// 这条行情该投给谁：由类型分档直接给出。投递范围的唯一出处 —— `symbol()` / `exchange()`
    /// 都由它派生。
    pub fn delivery(&self) -> Delivery<'_> {
        match self {
            MarketData::Symbol(d) => Delivery::Symbol(d.exchange(), d.symbol()),
            MarketData::Exchange(d) => Delivery::Exchange(d.exchange()),
            // 自定义事件的 scope 由构造方决定（for_symbol 定向 / new 广播）
            MarketData::Custom(c) => match c.scope() {
                Some((e, s)) => Delivery::Symbol(*e, s),
                None => Delivery::Broadcast,
            },
            MarketData::Clock => Delivery::Broadcast,
        }
    }

    /// 事件关联的 symbol（`None` = 无 symbol 维度）
    pub fn symbol(&self) -> Option<&Symbol> {
        self.delivery().symbol()
    }

    /// 事件来源交易所（`None` = 与交易所无关的全局事件）
    pub fn exchange(&self) -> Option<Exchange> {
        self.delivery().exchange()
    }
}

/// 一批历史 K 线（订阅时一次性推送）。(所, symbol) 是结构字段而不是从首根 K 线推导 ——
/// 否则空批没有归属、只能退化成广播。
#[derive(Debug, Clone)]
pub struct HistoryCandles {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub candles: Vec<Candle>,
}

// ============================================================================
// 账户私有事件
// ============================================================================

/// 账户私有事件：账户是**必填结构字段** —— "不知道归属的私有事件"在类型上不可表达。
///
/// 生产者：实盘适配层的私有路径（private WS / 账户轮询，标 [`AccountId::Live`]）与
/// 本地柜台 `PaperCounter`（标 `Paper(x)`）。二者发布**同一个类型**到同一条
/// AccountBus，消费者按 `account` 取自己的那份 —— 账户隔离由字段值而非总线拓扑保证。
///
/// 注：实盘适配层当前单账户，标签在发布点直接写 `AccountId::Live`；将来支持多实盘
/// 账户时把它提升为各适配 actor 的构造参数即可，事件模型不变。
#[derive(Debug, Clone)]
pub struct AccountEvent {
    /// 归属账户
    pub account: AccountId,
    pub exchange_ts: Timestamp,
    pub local_ts: Timestamp,
    pub data: AccountData,
}

/// 账户私有数据。定位精度在**类型**上分两档：币级（带 (所, symbol)）与所级（只带所）。
///
/// 分成两个类型而不是一张平铺枚举加一个「哪些变体有 symbol」的判断：状态层按档分派时
/// 不再有默认分支，新增一个账户事件必须先选档、再在该档的穷举 match 里写处理，漏了编译不过；
/// 币级状态（`SymbolState`）也收不到所级事件。
#[derive(Debug, Clone)]
pub enum AccountData {
    /// 币级：属于某个 (所, symbol)
    Symbol(SymbolAccountData),
    /// 所级：属于某个所的整个账户
    Exchange(ExchangeAccountData),
}

/// 币级账户事件（封闭枚举）
#[derive(Debug, Clone)]
pub enum SymbolAccountData {
    /// 交易所的订单回报
    OrderUpdate(OrderUpdate),
    /// 本地一次下单 / 撤单 REST 请求的结论（实盘出口发出；模拟柜台撤不到单时也发），见 [`OrderRequestResult`]
    OrderRequest(OrderRequestResult),
    /// 成交事件 (持仓的唯一增量来源，见 `SymbolState::seed_position` 的维护模型)
    Fill(Fill),
    /// 资费结算事件 (账户每次收/付资费时推送)
    FundingFee(FundingFee),
}

/// 本地一次下单 / 撤单请求的结论。和 [`OrderUpdate`] 分开：那是交易所说的订单状态，这是我们自己
/// 的 REST 调用得出的结论，两者到达顺序没有保证，本地跟踪表各按各的规则处理（见
/// `messaging::state::SymbolOrders`）。
///
/// 下单结果不确定（超时、5xx）时没有这个事件：实盘出口直接停机（见 `OrderGateway::place`）。
/// 撤单成功也没有：交易所随后会推 `Cancelled`（或在撤单生效前成交）。
#[derive(Debug, Clone, PartialEq)]
pub struct OrderRequestResult {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub client_order_id: String,
    pub outcome: RequestOutcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestOutcome {
    /// 下单 REST 返回成功：交易所已受理（订单号回填进跟踪表）
    Placed { order_id: crate::domain::OrderId },
    /// 下单确定没挂上：交易所明确拒单、请求没发出、或出口本地校验不过
    PlaceRejected { reason: String },
    /// 撤单请求失败（单可能已成交 / 已撤 / 还挂着，以交易所随后的订单回报为准）
    CancelFailed { reason: String },
}

/// 所级账户事件（封闭枚举）
#[derive(Debug, Clone)]
pub enum ExchangeAccountData {
    Balance(Balance),
    /// 账户希腊值 (按币种)。**账户持仓的风险读数**，归属账户侧
    Greeks(Greeks),
    /// 账户信息 (净值 + 总持仓名义价值)
    AccountInfo {
        exchange: Exchange,
        info: crate::domain::AccountInfo,
    },
    /// 本地持仓账本里该所**每个已跟踪币**的持仓价值，见 [`PositionValues`]
    PositionValues(PositionValues),
}

/// 本地持仓账本（`PositionLedger`：启动基线 + 之后全部实盘成交）里某所每个已跟踪币的
/// 持仓读数：数量 + 估值价（#759；#812 起带数量）。
///
/// **持仓与持仓价值在进程外的唯一来源**：风控闸门（策略的账户杠杆）和指标（看板上的持仓、
/// 杠杆、会话盈亏）读的都是这一份，不各自折叠成交、各自估值（#812）。
///
/// 与 [`ExchangeAccountData::AccountInfo`] 的 `notional`（交易所接口轮询的账户总持仓价值）
/// 是两个来源：这一份在任何一个币成交后立刻更新（不等轮询），按币分开给出，让单币策略
/// 能把自己那一项换成它自己最新的持仓。只含引擎跟踪的币（配置里的币），不含账户里别的仓位。
///
/// 实盘由账本在每笔成交后发该所的一份、每个对账节拍发全部所一份（价格变动）；某个有仓的币
/// 还没有盘口时不发（价值未知，不能当 0）—— 消费方沿用上一份读数，启动时没有读数即「未知」。
/// 模拟柜台与回测引擎随净值一起发同一读数（按自己的账本与估值价）。
#[derive(Debug, Clone, PartialEq)]
pub struct PositionValues {
    pub exchange: Exchange,
    /// 币 → 该币在本所的持仓读数
    pub legs: std::collections::HashMap<Symbol, LegValue>,
}

/// 一个币在一所的持仓读数。「有仓就有估值价」由构造保证（[`Self::new`]），读的一方不用再判。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegValue {
    size: Quantity,
    mark: Option<Price>,
}

impl LegValue {
    /// `size` 带符号持仓（币本位），`mark` 估值价（实盘是盘口中间价）。
    /// 有仓却没有价时返回 `None`：价值未知，不能当 0 给出去。空仓没有价也可以。
    pub fn new(size: Quantity, mark: Option<Price>) -> Option<Self> {
        (size.is_zero() || mark.is_some()).then_some(Self { size, mark })
    }

    /// 带符号持仓（币本位）
    pub fn size(&self) -> Quantity {
        self.size
    }

    /// 估值价；只有空仓且还没有盘口时为 `None`
    pub fn mark(&self) -> Option<Price> {
        self.mark
    }

    /// 带符号持仓价值 = 持仓 × 估值价（空仓为 0）
    pub fn signed_value(&self) -> f64 {
        self.mark.map_or(0.0, |m| to_f64(self.size * m))
    }

    /// 持仓价值 = |持仓| × 估值价（非负，空仓为 0）
    pub fn value(&self) -> f64 {
        self.signed_value().abs()
    }
}

impl SymbolAccountData {
    pub fn exchange(&self) -> Exchange {
        match self {
            SymbolAccountData::OrderUpdate(u) => u.exchange,
            SymbolAccountData::OrderRequest(r) => r.exchange,
            SymbolAccountData::Fill(f) => f.exchange,
            SymbolAccountData::FundingFee(fee) => fee.exchange,
        }
    }

    pub fn symbol(&self) -> &Symbol {
        match self {
            SymbolAccountData::OrderUpdate(u) => &u.symbol,
            SymbolAccountData::OrderRequest(r) => &r.symbol,
            SymbolAccountData::Fill(f) => &f.symbol,
            SymbolAccountData::FundingFee(fee) => &fee.symbol,
        }
    }
}

impl ExchangeAccountData {
    pub fn exchange(&self) -> Exchange {
        match self {
            ExchangeAccountData::Balance(b) => b.exchange,
            ExchangeAccountData::Greeks(g) => g.exchange,
            ExchangeAccountData::AccountInfo { exchange, .. } => *exchange,
            ExchangeAccountData::PositionValues(v) => v.exchange,
        }
    }
}

impl AccountData {
    /// 这条账户事件该投给谁：由类型分档直接给出。
    pub fn delivery(&self) -> Delivery<'_> {
        match self {
            AccountData::Symbol(d) => Delivery::Symbol(d.exchange(), d.symbol()),
            AccountData::Exchange(d) => Delivery::Exchange(d.exchange()),
        }
    }

    /// 事件关联的 symbol（`None` = 所级）
    pub fn symbol(&self) -> Option<&Symbol> {
        self.delivery().symbol()
    }

    /// 事件来源交易所（账户事件必有）
    pub fn exchange(&self) -> Exchange {
        match self {
            AccountData::Symbol(d) => d.exchange(),
            AccountData::Exchange(d) => d.exchange(),
        }
    }
}

// ============================================================================
// 统一视图（策略 / 状态层）
// ============================================================================

/// 一条事件的投递范围（借用事件里的 symbol，不分配）。
///
/// 消费者（executor / 回测 runner）声明的订阅范围是 `(交易所, symbol)` 的集合，
/// 而事件的定位精度有三档，正好对应本枚举的三个变体：
///
/// - [`Delivery::Symbol`]：行情与币级私有回报（BBO / OrderUpdate / Fill…）
/// - [`Delivery::Exchange`]：某个所的**所级读数**，无 symbol —— [`ExchangeAccountData`]
///   与 [`ExchangeMarketData`] 的全部变体。
///   只投给订了这个所的消费者：一个策略拿不到自己没订的所的净值与希腊值。
/// - [`Delivery::Broadcast`]：与交易所无关的全局事件（`Clock`、无 scope 的自定义事件）
///
/// 穷举 `match` 是刻意的（原则 P6）：新增一档投递范围时，分发层与 `accepts` 都会编译失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery<'a> {
    /// 定向到 (所, symbol)
    Symbol(Exchange, &'a Symbol),
    /// 定向到所
    Exchange(Exchange),
    /// 广播
    Broadcast,
}

impl<'a> Delivery<'a> {
    pub fn symbol(self) -> Option<&'a Symbol> {
        match self {
            Delivery::Symbol(_, s) => Some(s),
            Delivery::Exchange(_) | Delivery::Broadcast => None,
        }
    }

    pub fn exchange(self) -> Option<Exchange> {
        match self {
            Delivery::Symbol(e, _) | Delivery::Exchange(e) => Some(e),
            Delivery::Broadcast => None,
        }
    }
}

/// 策略与状态层的统一事件视图。
///
/// 总线上不存在这个类型（MarketBus 传 [`MarketEvent`]、AccountBus 传 [`AccountEvent`]），
/// 它在分发边界（IncomeProcessor → executor、回测引擎 → runner）构造，让
/// `Strategy::on_event` 保持单一入口。
#[derive(Debug, Clone)]
pub enum IncomeEvent {
    Market(MarketEvent),
    Account(AccountEvent),
}

impl IncomeEvent {
    /// 便捷构造：市场事件
    pub fn market(exchange_ts: Timestamp, local_ts: Timestamp, data: MarketData) -> Self {
        IncomeEvent::Market(MarketEvent {
            exchange_ts,
            local_ts,
            data,
        })
    }

    /// 便捷构造：账户事件
    pub fn account(
        account: AccountId,
        exchange_ts: Timestamp,
        local_ts: Timestamp,
        data: AccountData,
    ) -> Self {
        IncomeEvent::Account(AccountEvent {
            account,
            exchange_ts,
            local_ts,
            data,
        })
    }

    /// 事件关联的 symbol（`None` = 所级或全局事件）
    pub fn symbol(&self) -> Option<&Symbol> {
        self.delivery().symbol()
    }

    /// 事件来源交易所（`None` = 全局事件）
    pub fn exchange(&self) -> Option<Exchange> {
        self.delivery().exchange()
    }

    /// 这条事件该投给谁。
    ///
    /// **路由知识的单一出处**：每个变体的档位由 [`MarketData::delivery`] 的穷举 match
    /// 与 [`AccountData`] 的类型分档给出；实盘分发层与回测都经
    /// [`crate::messaging::SubscriptionScope::accepts`] 用它。账户归属也在那里判。
    pub fn delivery(&self) -> Delivery<'_> {
        match self {
            IncomeEvent::Market(m) => m.data.delivery(),
            IncomeEvent::Account(a) => a.data.delivery(),
        }
    }

    /// 获取交易所时间戳
    pub fn exchange_ts(&self) -> Timestamp {
        match self {
            IncomeEvent::Market(m) => m.exchange_ts,
            IncomeEvent::Account(a) => a.exchange_ts,
        }
    }

    /// 获取本地时间戳
    pub fn local_ts(&self) -> Timestamp {
        match self {
            IncomeEvent::Market(m) => m.local_ts,
            IncomeEvent::Account(a) => a.local_ts,
        }
    }
}

impl From<MarketEvent> for IncomeEvent {
    fn from(m: MarketEvent) -> Self {
        IncomeEvent::Market(m)
    }
}

impl From<AccountEvent> for IncomeEvent {
    fn from(a: AccountEvent) -> Self {
        IncomeEvent::Account(a)
    }
}

#[cfg(test)]
mod custom_event_tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct AlphaSignal {
        score: f64,
    }
    struct OtherType;

    fn wrap(c: CustomEvent) -> IncomeEvent {
        IncomeEvent::market(1, 1, MarketData::Custom(c))
    }

    /// 消费方按具体类型取回 payload；类型不符得 None —— 这是分发的实际粒度，
    /// 订阅者只对自己认识的类型起反应
    #[test]
    fn downcast_roundtrip_and_type_mismatch() {
        let ev = CustomEvent::new(AlphaSignal { score: 0.7 });
        assert_eq!(ev.get::<AlphaSignal>(), Some(&AlphaSignal { score: 0.7 }));
        assert!(
            ev.get::<OtherType>().is_none(),
            "类型不符必须得 None，而不是错误的值"
        );
        assert!(
            ev.name.contains("AlphaSignal"),
            "name 供日志定位: {}",
            ev.name
        );
    }

    /// 带 scope 的自定义事件走既有的 (exchange, symbol) 定向路由 —— 复用订阅关系，
    /// 只投给订阅了该 symbol 的 executor
    #[test]
    fn scoped_custom_event_routes_by_symbol() {
        let ev = wrap(CustomEvent::for_symbol(
            Exchange::Binance,
            "BTC".to_string(),
            AlphaSignal { score: 1.0 },
        ));
        assert_eq!(
            ev.delivery(),
            Delivery::Symbol(Exchange::Binance, &"BTC".to_string())
        );
    }

    /// 无 scope 的自定义事件广播给所有 executor
    #[test]
    fn unscoped_custom_event_broadcasts() {
        let ev = wrap(CustomEvent::new(AlphaSignal { score: 1.0 }));
        assert_eq!(ev.delivery(), Delivery::Broadcast);
    }
}

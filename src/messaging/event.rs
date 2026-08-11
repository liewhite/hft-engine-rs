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
/// # 两个方向、两条既有总线（不新增分发机制）
///
/// - **策略向外**：策略返回 [`crate::strategy::OutcomeEvent::Emit`]，事件随其他信号发布
///   到 Outcome 总线，外部处理者经 `SubscribeOutcome` 订阅（带账户归属）。**不会回流给
///   任何策略** —— 策略间不能直接通信，回环在结构上不可能；确需转发时由外部处理者显式
///   经入向入口注入，转发点可见可控。
/// - **外部向策略**：经 `ManagerActor` 的 `PublishCustomEvent` 注入 Market 总线，
///   按 scope 路由（见下）后进入订阅者的 `Strategy::on_event`。
///
/// # 路由与账户归属
///
/// - 带 `(exchange, symbol)` scope 的事件按既有订阅关系定向投递，无 scope 的广播 ——
///   复用 [`IncomeEvent::routing`] 的推导，无第二套路由。
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
    pub fn for_symbol<T: Any + Send + Sync>(exchange: Exchange, symbol: Symbol, payload: T) -> Self {
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

/// 公共市场数据（封闭枚举，引擎穷尽处理；开放扩展走 [`CustomEvent`]）
#[derive(Debug, Clone)]
pub enum MarketData {
    FundingRate(FundingRate),
    BBO(BBO),
    /// 公共成交印记 (市场匿名成交)；回测/模拟盘撮合的成交来源
    MarketTrade(MarketTrade),
    MarkPrice(MarkPrice),
    IndexPrice(IndexPrice),
    /// 交易所市场状态变更
    ExchangeStatus {
        exchange: Exchange,
        status: MarketStatus,
    },
    /// K线实时推送
    Candle(Candle),
    /// 历史K线批量数据（订阅时一次性推送；发布端保证非空，见 OKX business WS）
    HistoryCandles(Vec<Candle>),
    /// 融券券源读数 (借券费 + 可借量)；参考数据，策略在 on_event 里消费
    BorrowFee(BorrowFee),
    /// 汇率读数 (货币对)；参考数据，策略在 on_event 里消费
    ExchangeRate(ExchangeRate),
    /// 时钟事件 (用于超时检测等定时任务)
    Clock,
    /// 用户域自定义事件（唯一的开放扩展点，见 [`CustomEvent`]）。
    /// 框架的状态层（`StateManager`/`SymbolState`）对它一律 no-op —— 只运送，不理解。
    Custom(CustomEvent),
}

impl MarketData {
    /// 事件关联的 symbol（`None` = 无 symbol 维度，广播语义）
    pub fn symbol(&self) -> Option<&Symbol> {
        match self {
            MarketData::FundingRate(r) => Some(&r.symbol),
            MarketData::BBO(b) => Some(&b.symbol),
            MarketData::MarketTrade(t) => Some(&t.symbol),
            MarketData::MarkPrice(mp) => Some(&mp.symbol),
            MarketData::IndexPrice(ip) => Some(&ip.symbol),
            MarketData::Candle(c) => Some(&c.symbol),
            MarketData::HistoryCandles(cs) => cs.first().map(|c| &c.symbol),
            MarketData::BorrowFee(bf) => Some(&bf.symbol),
            // 自定义事件的 scope 由构造方决定（for_symbol 定向 / new 广播）
            MarketData::Custom(c) => c.scope().map(|(_, s)| s),
            MarketData::ExchangeStatus { .. }
            | MarketData::ExchangeRate(_)
            | MarketData::Clock => None,
        }
    }

    /// 事件来源交易所
    pub fn exchange(&self) -> Option<Exchange> {
        match self {
            MarketData::FundingRate(r) => Some(r.exchange),
            MarketData::BBO(b) => Some(b.exchange),
            MarketData::MarketTrade(t) => Some(t.exchange),
            MarketData::MarkPrice(mp) => Some(mp.exchange),
            MarketData::IndexPrice(ip) => Some(ip.exchange),
            MarketData::Candle(c) => Some(c.exchange),
            MarketData::HistoryCandles(cs) => cs.first().map(|c| c.exchange),
            MarketData::BorrowFee(bf) => Some(bf.exchange),
            MarketData::ExchangeRate(er) => Some(er.exchange),
            MarketData::ExchangeStatus { exchange, .. } => Some(*exchange),
            MarketData::Custom(c) => c.scope().map(|(e, _)| *e),
            MarketData::Clock => None,
        }
    }
}

// ============================================================================
// 账户私有事件
// ============================================================================

/// 账户私有事件：账户是**必填结构字段** —— "不知道归属的私有事件"在类型上不可表达。
///
/// 生产者：实盘适配层的私有路径（private WS / 账户轮询，标 [`AccountId::Live`]）与
/// 本地柜台 `PaperCounterActor`（标 `Paper(x)`）。二者发布**同一个类型**到同一条
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

/// 账户私有数据（封闭枚举）
#[derive(Debug, Clone)]
pub enum AccountData {
    OrderUpdate(OrderUpdate),
    /// 成交事件 (持仓的唯一增量来源，见 `SymbolState::seed_position` 的维护模型)
    Fill(Fill),
    Balance(Balance),
    /// 资费结算事件 (账户每次收/付资费时推送)
    FundingFee(FundingFee),
    /// 账户希腊值 (按币种)。**账户持仓的风险读数**，归属账户侧 —— 历史上它被广播给
    /// 全部账户是分类表漏行的意外行为（且无受益者：paper 策略因收不到 cashBal 恒不动作）
    Greeks(Greeks),
    /// 账户信息 (净值 + 总持仓名义价值)
    AccountInfo {
        exchange: Exchange,
        /// 账户净值 (balance + unrealized_pnl)
        equity: f64,
        /// 账户总持仓名义价值 (用于计算杠杆率)
        notional: f64,
    },
}

impl AccountData {
    /// 事件关联的 symbol（`None` = 账户级，投给该账户的全部 executor）
    pub fn symbol(&self) -> Option<&Symbol> {
        match self {
            AccountData::OrderUpdate(u) => Some(&u.symbol),
            AccountData::Fill(f) => Some(&f.symbol),
            AccountData::FundingFee(fee) => Some(&fee.symbol),
            AccountData::Balance(_) | AccountData::Greeks(_) | AccountData::AccountInfo { .. } => {
                None
            }
        }
    }

    /// 事件来源交易所
    pub fn exchange(&self) -> Option<Exchange> {
        match self {
            AccountData::OrderUpdate(u) => Some(u.exchange),
            AccountData::Fill(f) => Some(f.exchange),
            AccountData::FundingFee(fee) => Some(fee.exchange),
            AccountData::Balance(b) => Some(b.exchange),
            AccountData::Greeks(g) => Some(g.exchange),
            AccountData::AccountInfo { exchange, .. } => Some(*exchange),
        }
    }
}

// ============================================================================
// 统一视图（策略 / 状态层）
// ============================================================================

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

    /// 获取事件关联的 Symbol
    pub fn symbol(&self) -> Option<&Symbol> {
        match self {
            IncomeEvent::Market(m) => m.data.symbol(),
            IncomeEvent::Account(a) => a.data.symbol(),
        }
    }

    /// 获取事件来源交易所
    pub fn exchange(&self) -> Option<Exchange> {
        match self {
            IncomeEvent::Market(m) => m.data.exchange(),
            IncomeEvent::Account(a) => a.data.exchange(),
        }
    }

    /// 路由键: `Some` 表示按 (exchange, symbol) 定向路由, `None` 表示广播。
    ///
    /// **路由知识的单一出处**：实盘分发层（IncomeProcessor → executor）与回测
    /// （`StrategyRunner::accepts`）都由本方法推导 —— 新增事件变体时改 `symbol()`/
    /// `exchange()` 即可，没有第二份 match，也没有任何例外变体。
    ///
    /// 注意账户事件的"广播"仍限于**该账户**的 executor（分发层按 `account` 先过滤）。
    pub fn routing(&self) -> Option<(Exchange, Symbol)> {
        match (self.exchange(), self.symbol()) {
            (Some(e), Some(s)) => Some((e, s.clone())),
            _ => None,
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
        assert!(ev.get::<OtherType>().is_none(), "类型不符必须得 None，而不是错误的值");
        assert!(ev.name.contains("AlphaSignal"), "name 供日志定位: {}", ev.name);
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
        assert_eq!(ev.routing(), Some((Exchange::Binance, "BTC".to_string())));
    }

    /// 无 scope 的自定义事件广播给所有 executor（routing = None 即广播）
    #[test]
    fn unscoped_custom_event_broadcasts() {
        let ev = wrap(CustomEvent::new(AlphaSignal { score: 1.0 }));
        assert_eq!(ev.routing(), None);
    }
}

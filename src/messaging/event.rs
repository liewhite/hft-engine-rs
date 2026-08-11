use crate::domain::{Balance, BorrowFee, Candle, Exchange, ExchangeRate, Fill, FundingFee, FundingRate, Greeks, IndexPrice, MarkPrice, MarketStatus, MarketTrade, OrderUpdate, Symbol, Timestamp, BBO};
use std::any::Any;
use std::sync::Arc;

/// 用户域自定义事件：框架只负责运送与路由，不理解其内容。
///
/// 框架事件枚举（[`ExchangeEventData`] 其余变体）是**封闭**的 —— 引擎必须穷尽处理它们；
/// 本类型是枚举上唯一的**开放**扩展点：新事件类型只需定义一个 payload struct，
/// 不改框架任何代码。
///
/// # 两个方向、两条既有总线（不新增分发机制）
///
/// - **策略向外**：策略返回 [`crate::strategy::OutcomeEvent::Emit`]，事件随其他信号发布
///   到 Outcome 总线，外部处理者经 `SubscribeOutcome` 订阅（带账户归属）。**不会回流给
///   任何策略** —— 策略间不能直接通信，回环在结构上不可能；确需转发时由外部处理者显式
///   经入向入口注入，转发点可见可控。
/// - **外部向策略**：经 `ManagerActor` 的 `PublishCustomEvent` 注入 Income 总线，
///   按 scope 路由（见下）后进入订阅者的 `Strategy::on_event`。
///
/// # 路由与账户归属
///
/// - 带 `(exchange, symbol)` scope 的事件按既有订阅关系定向投递，无 scope 的广播 ——
///   复用 [`IncomeEvent::routing`] 的推导，无第二套路由。
/// - 自定义事件**没有账户归属**（如同行情）：实盘与模拟账户的策略都会收到。
///   需要区分时在 payload 里自带判别字段。
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

/// 统一的交易所事件
///
/// 设计原则：
/// - exchange_ts: 交易所推送的时间戳
/// - local_ts: 本地接收时间戳
/// - data: 具体的事件数据
#[derive(Debug, Clone)]
pub struct IncomeEvent {
    /// 交易所时间戳
    pub exchange_ts: Timestamp,
    /// 本地接收时间戳
    pub local_ts: Timestamp,
    /// 事件数据
    pub data: ExchangeEventData,
}

/// 事件数据类型
#[derive(Debug, Clone)]
pub enum ExchangeEventData {
    FundingRate(FundingRate),
    BBO(BBO),
    /// 公共成交印记 (市场匿名成交)；回测撮合的成交来源，实盘暂无该馈送
    MarketTrade(MarketTrade),
    MarkPrice(MarkPrice),
    IndexPrice(IndexPrice),
    // 注：持仓**基线**与**对账读数**不是事件、不在此枚举 ——
    // - 基线是投产握手的载荷（[`crate::messaging::PositionBaseline`]），由 `ManagerActor`
    //   在投产期一次 REST 快照产出，随注册消息/ExecutorArgs 点对点交给每个账本消费者
    //   （executor、对账镜像、观测镜像）。"只给特定消费者、只发一次"的数据不该上广播总线：
    //   历史上它作为总线事件存在时，需要 SkipExecutors 路由例外 + 引擎级去重集 + 对账层
    //   Fill 缓冲重放三套补偿机制。
    // - 对账读数只有一个消费者（`PositionReconcileActor`，自行轮询 REST），不经总线。
    // 交易所适配层的私有持仓推送（OKX `positions`、Hyperliquid `clearinghouseState`、
    // Binance `ACCOUNT_UPDATE.P`）都是**读数**而非基线，一律不得进入事件流。
    OrderUpdate(OrderUpdate),
    /// 成交事件 (用于乐观更新仓位)
    Fill(Fill),
    Balance(Balance),
    /// 资费结算事件 (账户每次收/付资费时推送)
    FundingFee(FundingFee),
    /// 账户希腊值 (按币种)
    Greeks(Greeks),
    /// 账户信息 (净值 + 总持仓名义价值)
    AccountInfo {
        exchange: Exchange,
        /// 账户净值 (balance + unrealized_pnl)
        equity: f64,
        /// 账户总持仓名义价值 (用于计算杠杆率)
        notional: f64,
    },
    /// 交易所市场状态变更
    ExchangeStatus {
        exchange: Exchange,
        status: MarketStatus,
    },
    /// K线实时推送
    Candle(Candle),
    /// 历史K线批量数据（订阅时一次性推送）
    HistoryCandles(Vec<Candle>),
    /// 融券券源读数 (借券费 + 可借量)；由外部数据源（IBKR snapshot 轮询）产出，策略在 on_event 里消费
    BorrowFee(BorrowFee),
    /// 汇率读数 (货币对)；由外部数据源（IBKR snapshot 轮询）产出，策略在 on_event 里消费
    ExchangeRate(ExchangeRate),
    /// 时钟事件 (用于超时检测等定时任务)
    Clock,
    /// 用户域自定义事件（唯一的开放扩展点，见 [`CustomEvent`]）。
    /// 框架的状态层（`StateManager`/`SymbolState`）对它一律 no-op —— 只运送，不理解。
    Custom(CustomEvent),
}

impl IncomeEvent {
    /// 获取事件关联的 Symbol
    pub fn symbol(&self) -> Option<&Symbol> {
        match &self.data {
            ExchangeEventData::FundingRate(rate) => Some(&rate.symbol),
            ExchangeEventData::BBO(bbo) => Some(&bbo.symbol),
            ExchangeEventData::MarketTrade(t) => Some(&t.symbol),
            ExchangeEventData::MarkPrice(mp) => Some(&mp.symbol),
            ExchangeEventData::IndexPrice(ip) => Some(&ip.symbol),
            ExchangeEventData::OrderUpdate(update) => Some(&update.symbol),
            ExchangeEventData::Fill(fill) => Some(&fill.symbol),
            ExchangeEventData::FundingFee(fee) => Some(&fee.symbol),
            ExchangeEventData::BorrowFee(bf) => Some(&bf.symbol),
            ExchangeEventData::Candle(candle) => Some(&candle.symbol),
            ExchangeEventData::HistoryCandles(candles) => candles.first().map(|c| &c.symbol),
            // 自定义事件的 scope 由构造方决定（for_symbol 定向 / new 广播）
            ExchangeEventData::Custom(c) => c.scope().map(|(_, s)| s),
            ExchangeEventData::Balance(_)
            | ExchangeEventData::Greeks(_)
            | ExchangeEventData::AccountInfo { .. }
            | ExchangeEventData::ExchangeStatus { .. }
            | ExchangeEventData::ExchangeRate(_)
            | ExchangeEventData::Clock => None,
        }
    }

    /// 获取事件来源交易所
    pub fn exchange(&self) -> Option<Exchange> {
        match &self.data {
            ExchangeEventData::FundingRate(rate) => Some(rate.exchange),
            ExchangeEventData::BBO(bbo) => Some(bbo.exchange),
            ExchangeEventData::MarketTrade(t) => Some(t.exchange),
            ExchangeEventData::MarkPrice(mp) => Some(mp.exchange),
            ExchangeEventData::IndexPrice(ip) => Some(ip.exchange),
            ExchangeEventData::OrderUpdate(update) => Some(update.exchange),
            ExchangeEventData::Fill(fill) => Some(fill.exchange),
            ExchangeEventData::Candle(candle) => Some(candle.exchange),
            ExchangeEventData::HistoryCandles(candles) => candles.first().map(|c| c.exchange),
            ExchangeEventData::Balance(bal) => Some(bal.exchange),
            ExchangeEventData::FundingFee(fee) => Some(fee.exchange),
            ExchangeEventData::Greeks(g) => Some(g.exchange),
            ExchangeEventData::BorrowFee(bf) => Some(bf.exchange),
            ExchangeEventData::ExchangeRate(er) => Some(er.exchange),
            ExchangeEventData::AccountInfo { exchange, .. } => Some(*exchange),
            ExchangeEventData::ExchangeStatus { exchange, .. } => Some(*exchange),
            ExchangeEventData::Custom(c) => c.scope().map(|(e, _)| *e),
            ExchangeEventData::Clock => None,
        }
    }

    /// 路由键: `Some` 表示按 (exchange, symbol) 定向路由, `None` 表示广播给所有策略。
    ///
    /// **路由知识的单一出处**：实盘分发层（IncomeProcessor -> executor）与回测
    /// （`StrategyRunner::accepts`）都由本方法推导 —— 新增事件变体时改 `symbol()`/
    /// `exchange()` 即可，不存在需要手工同步的第二份 match，也没有任何例外变体
    /// （历史上"基线/对账读数"两个 SkipExecutors 例外随它们退出事件枚举而消失）。
    ///
    /// 边角：空的 `HistoryCandles` 推不出 symbol、会落进广播 —— 无害（下游遍历零根
    /// K 线是 no-op），且发布端保证非空（空数组在发布前过滤）。
    pub fn routing(&self) -> Option<(Exchange, Symbol)> {
        match (self.exchange(), self.symbol()) {
            (Some(e), Some(s)) => Some((e, s.clone())),
            _ => None,
        }
    }

    /// 获取交易所时间戳
    pub fn exchange_ts(&self) -> Timestamp {
        self.exchange_ts
    }

    /// 获取本地时间戳
    pub fn local_ts(&self) -> Timestamp {
        self.local_ts
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
        IncomeEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: ExchangeEventData::Custom(c),
        }
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
        assert_eq!(
            ev.routing(),
            Some((Exchange::Binance, "BTC".to_string()))
        );
    }

    /// 无 scope 的自定义事件广播给所有 executor（routing = None 即广播）
    #[test]
    fn unscoped_custom_event_broadcasts() {
        let ev = wrap(CustomEvent::new(AlphaSignal { score: 1.0 }));
        assert_eq!(ev.routing(), None);
    }
}

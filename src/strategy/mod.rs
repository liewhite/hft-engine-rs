mod funding_arb;
mod gamma_scalp;
mod spread_arb;
mod view;

pub use spread_arb::{SpreadArbConfig, SpreadArbStrategy, MIN_EXCHANGES_PER_SYMBOL};
pub use funding_arb::{
    FundingArbConfig, FundingArbStrategy, PairConfig as FundingArbPairConfig, SettledFundingWindow,
    FUNDING_WINDOW_MS,
};
pub use gamma_scalp::GammaScalpStrategy;
pub use view::{StrategyView, SymbolView};

use crate::domain::{Exchange, Order, Symbol, Timestamp};
use crate::observability::Gauge;
use crate::exchange::{AccountStream, SubscriptionKind};
use crate::messaging::{CustomEvent, IncomeEvent};
use std::collections::{HashMap, HashSet};

/// 策略输出的信号
#[derive(Debug, Clone)]
pub enum OutcomeEvent {
    /// 下单信号（一次决策可包含多个关联订单）
    PlaceOrders {
        /// 关联订单列表
        orders: Vec<Order>,
        /// 信号意图描述，如 "spread_open | spread=0.30% | qty=10"
        comment: String,
    },
    /// 撤单信号（按客户端订单号撤，不等交易所订单号）。
    ///
    /// 框架只对下单已受理、还开着、且没有撤单在途的单发出，其余丢弃（见 `SymbolOrders::mark_cancel_sent`），
    /// 策略要在之后的事件里重新要求；撤单失败以
    /// [`crate::messaging::RequestOutcome::CancelFailed`] 回报，记在单的 `cancel` 上。
    CancelOrder {
        exchange: Exchange,
        symbol: Symbol,
        client_order_id: String,
    },
    /// 向外发布自定义事件（见 [`CustomEvent`] 的方向与路由文档）。
    ///
    /// 随其他信号发布到 Outcome 总线，外部观察者订阅 `ManagerHandle::outcome_bus()`、
    /// 按类型 `get::<T>()` 消费；下单/撤单处理器（实盘出口与模拟柜台）对它 no-op。
    /// **不会回流给任何策略** —— 确需喂给策略的事件走
    /// `ManagerHandle::publish_custom_event` 入向入口。
    Emit(CustomEvent),
}

/// 策略 trait
///
/// 用户实现此 trait 来定义自己的策略逻辑
/// 策略是纯函数式的：接收事件和状态，返回要执行的动作
pub trait Strategy: Send + Sync {
    /// 策略需要订阅的公共数据流
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>>;

    /// 策略需要的账户类数据流（见 [`AccountStream`]，如 OKX 的希腊值）。投产时框架在对应的所
    /// 起它们（同一所同一种只起一份，多个策略共用）；该所不提供或没配凭证则拒绝投产。
    /// 净值、持仓、成交、资金费流水不在此列：它们框架总是提供。默认不要。
    fn account_streams(&self) -> HashMap<Exchange, HashSet<AccountStream>> {
        HashMap::new()
    }

    /// 处理事件，可产出零到多个信号。
    ///
    /// **唯一的事件入口**：行情（BBO/Trades/MarkPrice/…）、私有回报（OrderUpdate/Fill）、
    /// 账户读数（Balance/AccountInfo/Greeks）、券源与汇率（BorrowFee/ExchangeRate）、
    /// Clock 全部经此到达，策略按需 match 自己关心的变体。此前 BorrowFee/ExchangeRate
    /// 走独立的 typed 回调 —— 同一事件流两套分发风格，策略在 on_event 里 match 这两类
    /// 永远收不到且编译器不报错。
    ///
    /// `view` 是**本策略订阅范围内**的状态视图（见 [`StrategyView`]），按值传入且不可
    /// 跨事件持有 —— 引擎状态容器不作为策略契约的一部分暴露。
    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent>;

    /// 策略自己的观测读数（如信号输入、当前判断、是否停止交易），只读快照。
    ///
    /// 实盘里框架每个 Clock 拍取一次，交给指标出口（`Metrics`，配了 pushgateway 就推出去，
    /// 读数自动加上实例账户标签）；实例撤下后它的读数随之消失。`now` 是这一拍的本地时刻。
    /// 名字与标签名须合 Prometheus 格式、不用框架的 `hft_` 前缀，名字加标签不重复；不合格的行
    /// 框架丢掉并记 warn（见 [`Gauge::validate`]）。名字请带策略前缀，免得与别的策略撞名。默认没有读数。
    fn gauges(&self, _now: Timestamp) -> Vec<Gauge> {
        Vec::new()
    }
}

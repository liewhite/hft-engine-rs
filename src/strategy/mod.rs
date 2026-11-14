mod funding_arb;
mod view;

pub use funding_arb::{
    FundingArbConfig, FundingArbStrategy, PairConfig as FundingArbPairConfig, SettledFundingWindow,
    FUNDING_WINDOW_MS,
};
pub use view::{StrategyView, SymbolView};

use crate::domain::{Exchange, Order, OrderIntent, Symbol, Timestamp};
use crate::exchange::{AccountStream, SubscriptionKind};
use crate::messaging::IncomeEvent;
use crate::observability::Gauge;
use std::collections::{HashMap, HashSet};

/// 策略产出的信号。由框架（`StrategyRunner`）处理成发往出口的 [`OutcomeEvent`]：
/// 下单意图过范围闸门、赋号、取整；撤单按跟踪表补上所在的所。
#[derive(Debug, Clone)]
pub enum Signal {
    /// 下单（一次决策可包含多个关联订单）：只给意图（[`OrderIntent`]，没有订单号、未取整）
    PlaceOrders {
        orders: Vec<OrderIntent>,
        /// 信号意图描述，如 "spread_open | spread=0.30% | qty=10"
        comment: String,
    },
    /// 撤本策略的一张单（按客户端订单号，不等交易所订单号）。在哪个所由框架从跟踪表查，策略不填，
    /// 也就填不错。
    ///
    /// 框架只对下单已受理、还开着（收到的成交也没加满单量）、且没有撤单在途的单发出，其余丢弃（见
    /// `SymbolOrders::mark_cancel_sent`），策略要在之后的事件里重新要求；撤单失败以
    /// [`crate::messaging::RequestOutcome::CancelFailed`] 回报，记在单的撤单状态上（[`crate::messaging::CancelAttempt::Failed`]）。
    CancelOrder {
        symbol: Symbol,
        client_order_id: String,
    },
}

/// 框架发往出向总线的事件（由 [`Signal`] 处理而来，见 `StrategyRunner::on_event`）
#[derive(Debug, Clone)]
pub enum OutcomeEvent {
    /// 要发出的单：已过范围闸门、已赋号、已按交易所精度取整，且已登记跟踪表
    PlaceOrders { orders: Vec<Order>, comment: String },
    /// 要发出的撤单（所取自跟踪表）
    CancelOrder {
        exchange: Exchange,
        symbol: Symbol,
        client_order_id: String,
    },
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
    /// 账户读数（Balance/AccountInfo/Greeks）、Clock 全部经此到达，策略按需 match 自己关心的变体。
    ///
    /// `view` 是**本策略订阅范围内**的状态视图（见 [`StrategyView`]），按值传入且不可
    /// 跨事件持有 —— 引擎状态容器不作为策略契约的一部分暴露。
    ///
    /// 下单意图只能落在本策略订阅的 (所, 币) 上（[`Self::public_streams`] 声明的范围）：范围外的
    /// 单框架不发，整机停机（策略写错了，发出去就是不在跟踪表、不在对账名单里的真单）。
    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<Signal>;

    /// 策略自己的观测读数（如信号输入、当前判断、是否停止交易），只读快照。
    ///
    /// 实盘里框架每个 Clock 拍取一次，交给指标出口（`Metrics`，配了 pushgateway 就推出去，
    /// 读数自动加上实例账户标签）。`view` 是此刻的状态（同 `on_event`），`now` 是这一拍的本地时刻。
    /// 名字与标签名须合 Prometheus 格式、不用框架的 `hft_` 前缀，名字加标签不重复；不合格的行
    /// 框架丢掉并记 warn（见 [`Gauge::validate`]）。名字请带策略前缀，免得与别的策略撞名。默认没有读数。
    fn gauges(&self, _view: StrategyView<'_>, _now: Timestamp) -> Vec<Gauge> {
        Vec::new()
    }
}

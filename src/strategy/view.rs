//! 策略能看到的状态视图。
//!
//! # 这一层解决什么，不解决什么
//!
//! **解决的是接口过宽**（`docs/architecture.md` 原则 P3）：`StateManager` + `SymbolState`
//! 合计 39 个公开方法，而仓内全部策略的实测调用面只有 11 个。差距一大就有两件事跟着来 ——
//! 消费者读到不该读的，以及往状态里加字段会**自动扩大**所有策略的可见面而无人察觉。
//!
//! **不解决 symbol 越界**，因为那本来就不存在：`StrategyRunner` 用策略自己
//! `public_streams()` 推导出的 symbol 集合去建 `StateManager`，所以它能查到的 symbol
//! 本来就只有自己订的那些。（架构文档 V2 初稿写过"策略能看到其他 symbol 的状态"，
//! 那是错的，已更正。）
//!
//! **确实收窄的一处越界**是账户级数据：`equity` / `account_info` / `greeks` 按交易所索引，
//! 而此前不按订阅范围过滤 —— 策略可以读到它压根没订阅的交易所的净值。本视图按
//! 本实例订阅的交易所裁剪。
//!
//! # 裁剪一律用 `Option`，不用默认值
//!
//! 范围外返回 `None` 而不是 0 / 空 —— 见 `docs/architecture.md` 品味 1.3。
//! 这也沿用了 `StateManager::symbol_state` 既有的形状，故策略侧改动很小。

use crate::domain::{AccountInfo, Exchange, Greeks, Symbol};
use crate::messaging::{PendingOrder, StateManager, SymbolState};
use std::collections::HashSet;

/// 单个 symbol 的只读视图。方法集 = 仓内策略的实测调用面，不多给。
pub struct SymbolView<'a>(&'a SymbolState);

impl<'a> SymbolView<'a> {
    /// 某所盘口。返回 `None` = 该所这个 symbol 的盘口尚未到达，**不是**"价格为 0"。
    pub fn bbo(&self, exchange: Exchange) -> Option<&'a crate::domain::BBO> {
        self.0.bbo(exchange)
    }

    /// 某所仓位（币本位带符号）。
    ///
    /// # 已知盲区（不是本次引入，如实声明）
    ///
    /// 该所若未配置凭证（无账户），投产期不会为它拉基线，这条腿处于"未 seed"状态，
    /// 本方法返回 0。对本引擎而言这是对的 —— 没有账户就下不了单，也收不到成交，
    /// 引擎侧仓位确实是 0。但**交易所上人工持有的存量看不见**。
    /// 需要区分"未知"与"空仓"的消费者（对账、对外快照）走
    /// [`crate::messaging::SymbolPositions::seeded_positions`]，那条路会把未 seed 的腿排除。
    pub fn position_size(&self, exchange: Exchange) -> f64 {
        self.0.position_size(exchange)
    }

    /// 多空仓位大小：(多头总量, 空头总量)，后者为负
    pub fn position_sizes(&self) -> (f64, f64) {
        self.0.position_sizes()
    }

    /// 是否有未完成订单
    pub fn has_pending_orders(&self) -> bool {
        self.0.has_pending_orders()
    }

    /// 全部未完成订单
    pub fn pending_orders(&self) -> impl Iterator<Item = &'a PendingOrder> {
        self.0.pending_orders()
    }
}

/// 策略在 [`crate::strategy::Strategy::on_event`] 里能看到的全部状态。
///
/// 由 `StrategyRunner` 构造，生命周期绑在一次策略步上 —— 策略不能把它存起来跨事件用，
/// 这正是想要的：策略的记忆应当放在它自己的字段里，而不是攥着引擎状态的引用。
pub struct StrategyView<'a> {
    state: &'a StateManager,
    /// 本实例订阅的交易所（账户级数据按它裁剪）
    exchanges: &'a HashSet<Exchange>,
}

impl<'a> StrategyView<'a> {
    pub(crate) fn new(state: &'a StateManager, exchanges: &'a HashSet<Exchange>) -> Self {
        Self { state, exchanges }
    }

    /// 某 symbol 的视图。返回 `None` = 本策略没订阅这个 symbol。
    pub fn symbol(&self, symbol: &Symbol) -> Option<SymbolView<'a>> {
        self.state.symbol_state(symbol).map(SymbolView)
    }

    /// 某所账户净值。`None` = 该所未订阅、或净值读数尚未到达。
    ///
    /// 两种 `None` 刻意不区分：对策略而言都是"这个数现在不能用"，而区分它们会诱使
    /// 调用方写出"未订阅就当 0"的分支 —— 那正是品味 1.3 要消灭的。
    pub fn equity(&self, exchange: Exchange) -> Option<f64> {
        self.scoped(exchange).and_then(|_| self.state.equity(exchange))
    }

    /// 某所账户信息（净值 + 总持仓名义价值，原子读取）
    pub fn account_info(&self, exchange: Exchange) -> Option<&'a AccountInfo> {
        self.scoped(exchange)
            .and_then(|_| self.state.account_info(exchange))
    }

    /// 某所某币种的希腊值（delta 已含现货余额修正）
    pub fn greeks(&self, exchange: Exchange, ccy: &str) -> Option<Greeks> {
        self.scoped(exchange)
            .and_then(|_| self.state.greeks(exchange, ccy))
    }

    /// 订阅范围过滤：不在范围内的交易所一律 `None`
    fn scoped(&self, exchange: Exchange) -> Option<()> {
        self.exchanges.contains(&exchange).then_some(())
    }
}

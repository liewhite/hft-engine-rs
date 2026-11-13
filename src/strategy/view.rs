//! 策略视图 —— 策略能看到的**全部**状态。
//!
//! # 为什么不直接给 `StateManager`
//!
//! `StateManager` 是引擎的状态容器：它同时服务策略、观测层、持仓账本、executor。
//! 把它整个作为 `Strategy::on_event` 的参数，意味着**引擎内部每加一个读方法，
//! 策略契约就自动变宽一次** —— `symbol_states()`（全量遍历）、`seeded_positions()`
//! 都是为别的消费者加的，却一并进了策略的可见面。反过来，想改动 `StateManager`
//! 的任何方法都要担心波及策略实现。这正是 `docs/architecture.md` 原则 P3
//! （接口只暴露消费者需要的）说的那种漂移。
//!
//! 视图把契约钉死成实测的调用面：策略读不到的东西，编译期就不存在。
//!
//! # 视图**不做**范围裁剪
//!
//! 底层 `StateManager` 里本来就只有订阅范围内的读数 —— 事件在**分发层**按
//! [`crate::messaging::Delivery`] 过滤（`StrategyRunner::accepts` 与
//! `IncomeProcessor` 同据一份判据），范围外的东西根本进不来。
//!
//! 视图初版曾在这里按订阅交易所再裁剪一遍账户读数。那是错的，两个原因：
//! 同一条"什么算范围内"的判据成了第三处实现（原则 P1/P4）；而且它**挡不住**——
//! 越界的读数照样随 `&IncomeEvent` 推给策略，payload 里就带着净值，
//! 只堵查询不堵投递等于没堵（品味 1.5：只堵一半却声称堵住了，比不堵更糟）。
//!
//! 正确的位置是分发层，改在那里之后这里就无事可做了。
//!
//! # 取不到返回 `None`，不返回默认值
//!
//! 见品味 1.3：净值取不到与净值为零是两回事，前者伪装成后者会让杠杆闸门直接放行。

use crate::domain::{AccountInfo, Exchange, Greeks, Quantity, Symbol, BBO};
use crate::messaging::{PendingOrder, StateManager, SymbolState};
use std::collections::HashMap;

/// 策略视图：本策略订阅范围内的状态。
///
/// 由 [`crate::engine::StrategyRunner`] 在每次分发事件时借出，生命周期不超过一次
/// `on_event` 调用 —— 策略无法把它存起来跨事件持有（那会读到陈旧状态）。
#[derive(Clone, Copy)]
pub struct StrategyView<'a> {
    state: &'a StateManager,
}

impl<'a> StrategyView<'a> {
    pub fn new(state: &'a StateManager) -> Self {
        Self { state }
    }

    /// 某个 symbol 的跨所全貌。范围外（未订阅）返回 `None`。
    pub fn symbol(&self, symbol: &Symbol) -> Option<SymbolView<'a>> {
        self.state.symbol_state(symbol).map(SymbolView::new)
    }

    /// 某所账户净值。读数尚未到达返回 `None`。
    pub fn equity(&self, exchange: Exchange) -> Option<f64> {
        self.state.account_view().equity(exchange)
    }

    /// 某所账户信息（净值 + 名义价值，原子写入）
    pub fn account_info(&self, exchange: Exchange) -> Option<&'a AccountInfo> {
        self.state.account_view().account_info(exchange)
    }

    /// 本地持仓账本里该所各币的持仓读数（数量 + 估值价），含别的策略实例的币。
    /// 账本还没发过该所读数时返回 `None`（未知，不是 0）。见 [`crate::messaging::PositionValues`]。
    pub fn position_values(
        &self,
        exchange: Exchange,
    ) -> Option<&'a HashMap<Symbol, crate::messaging::LegValue>> {
        self.state.account_view().position_values(exchange)
    }

    /// 某所某币种的希腊值（delta 已按现货余额修正）
    pub fn greeks(&self, exchange: Exchange, ccy: &str) -> Option<Greeks> {
        self.state.account_view().greeks(exchange, ccy)
    }
}

/// 单个 symbol 的跨所视图。
///
/// 方法集就是实测的策略调用面。`SymbolState` 上其余的读方法（`mark_price` / 三个投影的直通
/// 访问器…）没有策略消费者，是观测层与 executor 用的，不在此暴露。
#[derive(Clone, Copy)]
pub struct SymbolView<'a> {
    state: &'a SymbolState,
}

impl<'a> SymbolView<'a> {
    fn new(state: &'a SymbolState) -> Self {
        Self { state }
    }

    /// 某所盘口。未到达返回 `None`。
    pub fn bbo(&self, exchange: Exchange) -> Option<&'a BBO> {
        self.state.market().bbo(exchange)
    }

    /// 某所持仓大小（带符号，正多负空）。
    ///
    /// # 没有"未查询到"这个状态
    ///
    /// 作用域内的每条 (所, symbol)，在策略消费第一条事件**之前**就已经有持仓记录了 ——
    /// executor 出生时由投产握手写满（见 `manager/provisioning.rs` 的
    /// `AccountBinding::into_baselines`）。所以这里返回的 `0.0` 永远是"确定空仓"，不会是"还不知道"。
    ///
    /// 这条不变量是刻意建立的：此前缺记录的腿要靠 `Option` 让调用方分辨，而那个分支
    /// 写对了是冗余、写错了（当它空仓照常开仓）是事故。把状态消灭掉，比让每个调用点
    /// 去检查它更可靠（品味 1.1）。
    ///
    /// 作用域**外**的所返回 0 —— 那是策略问了自己没订阅的东西，与读未订阅 symbol
    /// 的盘口同类，属调用方的 bug。
    pub fn position_size(&self, exchange: Exchange) -> Quantity {
        self.state.position_book().position_size(exchange)
    }

    /// 跨所的 (多头总量, 空头总量)：正向持仓之和为正，负向持仓之和为负。
    pub fn position_sizes(&self) -> (Quantity, Quantity) {
        self.state.position_book().position_sizes()
    }

    /// 本 symbol 是否有未完成订单
    pub fn has_pending_orders(&self) -> bool {
        self.state.order_book().has_pending()
    }

    /// 遍历本 symbol 的未完成订单
    pub fn pending_orders(&self) -> impl Iterator<Item = &'a PendingOrder> {
        self.state.order_book().iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::StateManager;

    const SYM: &str = "BTC";

    /// 注册范围外的 symbol 拿不到视图 —— 而不是拿到一个空状态。
    ///
    /// 空状态会让策略以为该 symbol 空仓、无挂单，进而照常下单（品味 1.3）。
    #[test]
    fn symbols_outside_the_registered_set_are_not_visible() {
        let m = StateManager::new(&[SYM.to_string()]);
        let view = StrategyView::new(&m);
        assert!(view.symbol(&SYM.to_string()).is_some());
        assert!(view.symbol(&"ETH".to_string()).is_none());
    }
}

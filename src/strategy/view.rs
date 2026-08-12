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
//! # 裁剪只做一件事：账户数据按订阅交易所裁剪
//!
//! per-symbol 数据（行情/持仓/挂单）**已经**是裁剪过的 —— `StrategyRunner::accepts`
//! 按 `(exchange, symbol)` 过滤事件，`StateManager` 里压根不会出现范围外的读数。
//! 视图不再重复过滤一遍：同一条"什么算范围内"的判据只能有一处（原则 P1），
//! 在视图里再写一遍就成了第二份、且会与 `accepts` 各自演化。
//!
//! 真正没被裁剪的是**账户级读数**：`Balance` / `AccountInfo` / `Greeks` 没有路由键，
//! `accepts` 一律放行，于是策略能读到自己压根没订阅的交易所的净值与希腊值。
//! 这是确实存在的越界，由 [`StrategyView`] 按订阅交易所集合堵住。
//!
//! # 范围外返回 `None`，不返回默认值
//!
//! 见品味 1.3：净值取不到与净值为零是两回事，前者伪装成后者会让杠杆闸门直接放行。

use crate::domain::{AccountInfo, Exchange, Greeks, Symbol, BBO};
use crate::messaging::{PendingOrder, StateManager, SymbolState};
use std::collections::HashSet;

/// 策略视图：本策略订阅范围内的状态。
///
/// 由 [`crate::engine::StrategyRunner`] 在每次分发事件时借出，生命周期不超过一次
/// `on_event` 调用 —— 策略无法把它存起来跨事件持有（那会读到陈旧状态）。
#[derive(Clone, Copy)]
pub struct StrategyView<'a> {
    state: &'a StateManager,
    /// 本策略订阅的交易所集合，账户级读数的裁剪依据
    exchanges: &'a HashSet<Exchange>,
}

impl<'a> StrategyView<'a> {
    /// 构造视图。`exchanges` 应当来自策略自己的 `public_streams()`。
    pub fn new(state: &'a StateManager, exchanges: &'a HashSet<Exchange>) -> Self {
        Self { state, exchanges }
    }

    /// 某个 symbol 的跨所全貌。范围外（未订阅）返回 `None`。
    pub fn symbol(&self, symbol: &Symbol) -> Option<SymbolView<'a>> {
        self.state.symbol_state(symbol).map(SymbolView::new)
    }

    /// 某所账户净值。未订阅该所、或读数尚未到达，都返回 `None`。
    pub fn equity(&self, exchange: Exchange) -> Option<f64> {
        self.subscribed(exchange)?;
        self.state.account_view().equity(exchange)
    }

    /// 某所账户信息（净值 + 名义价值，原子写入）。裁剪同 [`Self::equity`]。
    pub fn account_info(&self, exchange: Exchange) -> Option<&'a AccountInfo> {
        self.subscribed(exchange)?;
        self.state.account_view().account_info(exchange)
    }

    /// 某所某币种的希腊值（delta 已按现货余额修正）。裁剪同 [`Self::equity`]。
    pub fn greeks(&self, exchange: Exchange, ccy: &str) -> Option<Greeks> {
        self.subscribed(exchange)?;
        self.state.account_view().greeks(exchange, ccy)
    }

    /// 订阅范围判据。返回 `Option` 而非 `bool` 是为了让上面三个方法用 `?` 直接短路，
    /// 不必每处写一遍 `if !... { return None }`。
    fn subscribed(&self, exchange: Exchange) -> Option<()> {
        if self.exchanges.contains(&exchange) {
            return Some(());
        }
        tracing::warn!(
            %exchange,
            "策略读取了未订阅交易所的账户读数，返回 None（订阅范围由 public_streams 决定）"
        );
        None
    }
}

/// 单个 symbol 的跨所视图。
///
/// 方法集就是实测的策略调用面。`SymbolState` 上其余的读方法（`mark_price` /
/// `best_long_exchange` / `exposure` / 三个投影的直通访问器…）没有策略消费者，
/// 是观测层与 executor 用的，不在此暴露。
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
        self.state.bbo(exchange)
    }

    /// 某所持仓（带符号，正多负空）。
    ///
    /// # 未 seed 的腿报 0 是否算"未知伪装成默认值"
    ///
    /// 不算，已核实：投产握手对**每个有 `AccountClient` 的所**都拉基线，拉取失败直接拒绝
    /// 启动（见 `manager/provisioning.rs` 的 `fetch_baselines`）—— 所以实盘可下单的所必然
    /// 已 seed。没有 `AccountClient` 的所（只订公共行情、无凭证）确实不 seed，但引擎在那里
    /// 从来不会、也不能下单，"本地持仓 0" 是事实而非缺数据。
    pub fn position_size(&self, exchange: Exchange) -> f64 {
        self.state.position_size(exchange)
    }

    /// 跨所的 (多头总量, 空头总量)：正向持仓之和为正，负向持仓之和为负。
    pub fn position_sizes(&self) -> (f64, f64) {
        self.state.position_sizes()
    }

    /// 本 symbol 是否有未完成订单
    pub fn has_pending_orders(&self) -> bool {
        self.state.has_pending_orders()
    }

    /// 遍历本 symbol 的未完成订单
    pub fn pending_orders(&self) -> impl Iterator<Item = &'a PendingOrder> {
        self.state.pending_orders()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AccountId;
    use crate::messaging::{AccountData, IncomeEvent};

    const SUBSCRIBED: Exchange = Exchange::OKX;
    const OUTSIDE: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn manager() -> StateManager {
        let mut m = StateManager::new(&[SYM.to_string()], 0);
        for exchange in [SUBSCRIBED, OUTSIDE] {
            m.apply(&IncomeEvent::account(
                AccountId::Live,
                0,
                0,
                AccountData::AccountInfo {
                    exchange,
                    equity: 1000.0,
                    notional: 500.0,
                },
            ));
        }
        m
    }

    fn scope() -> HashSet<Exchange> {
        HashSet::from([SUBSCRIBED])
    }

    /// 范围外 symbol 拿不到视图（而不是拿到一个空状态）
    #[test]
    fn symbols_outside_the_subscription_are_not_visible() {
        let m = manager();
        let scope = scope();
        let view = StrategyView::new(&m, &scope);
        assert!(view.symbol(&SYM.to_string()).is_some());
        assert!(
            view.symbol(&"ETH".to_string()).is_none(),
            "未订阅的 symbol 必须是 None，不能是空状态 —— 空状态会让策略以为该 symbol 空仓"
        );
    }

    /// **越界防线**：账户级读数没有路由键，`accepts` 一律放行，所以 `StateManager` 里
    /// 确实存着未订阅所的净值。视图必须把它挡掉，否则策略的杠杆闸门会拿别人的净值算。
    #[test]
    fn account_reads_are_cropped_to_subscribed_exchanges() {
        let m = manager();
        let scope = scope();
        let view = StrategyView::new(&m, &scope);

        assert_eq!(view.equity(SUBSCRIBED), Some(1000.0));
        assert!(view.account_info(SUBSCRIBED).is_some());

        assert!(
            m.account_view().equity(OUTSIDE).is_some(),
            "前提：底层状态里确实有未订阅所的读数，否则这条测试测了个空"
        );
        assert_eq!(view.equity(OUTSIDE), None, "未订阅所的净值不得可见");
        assert!(view.account_info(OUTSIDE).is_none(), "未订阅所的账户信息不得可见");
        assert!(view.greeks(OUTSIDE, "BTC").is_none(), "未订阅所的希腊值不得可见");
    }
}

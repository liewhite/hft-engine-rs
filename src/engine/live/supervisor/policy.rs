//! 晋升 / 降级判据的**扩展点**。
//!
//! 框架负责：维护每个 symbol 的模拟与实盘表现记录、按节拍询问判据、执行晋升（拉起实盘策略
//! 实例）与降级（撤下实例并立即平掉实盘仓位）。
//!
//! 判据本身由使用者实现 [`PromotionPolicy`]，框架不预设任何阈值。
//!
//! # 写判据时务必知道的两件事
//!
//! 1. **多重比较**：同时有 N 个 symbol 在跑模拟，即使策略毫无 edge，任意时刻约一半的模拟
//!    盈利，而**最好的那几个几乎必然看起来很赚**（N 个样本的最大值约在数个 σ 上）。以
//!    "盈利 > 0" 晋升等于系统性地在幸运跑完之后进场，紧接着均值回复；再叠加"实盘亏损就
//!    降级"，会构成高位晋升、低位降级的负 alpha 循环。判据需要统计功效（最少往返笔数 +
//!    显著性），必要时加样本外确认。
//! 2. **模拟成交偏乐观**：本地柜台不建队列位置模型，挂单在真实盘口要排队，被判定成交的
//!    单子在实盘可能排不到。成交**价格**是对的，成交**机会**偏多，因此模拟盈亏系统性偏高，
//!    门槛要留余量。晋升后同一 symbol 上模拟与实盘并行，两边成交率之差正是校准这一偏差的
//!    数据 —— 可以用它反过来修正门槛。

use crate::domain::{Symbol, Timestamp};

use super::record::SymbolRecord;

/// 判据对某个 symbol 能看到的全部事实：两个账户的表现 + 时间。
///
/// 此前叫 `SymbolView`，与 [`crate::strategy::SymbolView`]（策略的单 symbol 状态视图）
/// 重名 —— 两者毫无关系，却都从各自模块的顶层导出，同时 `use` 两个模块就会撞名
/// （`docs/architecture.md` V10）。改名的是本类型：它的消费者只有 `PromotionPolicy`
/// 的实现者，波及面比刚随 v0.13.0 发出去的策略契约小得多。
pub struct SymbolPerformance<'a> {
    pub symbol: &'a Symbol,
    /// 模拟账户的表现（常驻运行）
    pub paper: &'a SymbolRecord,
    /// 实盘账户的表现；`None` = 该 symbol 当前未开实盘
    pub live: Option<&'a SymbolRecord>,
    /// 实盘开启的时刻（毫秒）；`None` = 未开
    pub live_since: Option<Timestamp>,
    /// 当前时刻（毫秒）
    pub now: Timestamp,
}

impl SymbolPerformance<'_> {
    /// 实盘是否已开启
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// 实盘已运行时长（毫秒）；未开则为 0
    pub fn live_elapsed_ms(&self) -> u64 {
        self.live_since
            .map(|t| self.now.saturating_sub(t))
            .unwrap_or(0)
    }
}

/// 判据的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 维持现状
    Hold,
    /// 开启实盘（当前未开时有效）
    Promote,
    /// 关闭实盘并立即平掉实盘仓位（当前已开时有效）
    Demote,
}

/// 晋升 / 降级判据。
///
/// `&mut self` 是有意的：判据可以自己攒状态（例如记录"信号首次出现的时刻"以实现样本外
/// 确认的冷却期）。框架保证同一 symbol 的调用是串行的。
pub trait PromotionPolicy: Send + 'static {
    /// 对一个 symbol 作出决定。被框架按节拍（Clock）逐 symbol 调用。
    ///
    /// 返回与当前状态矛盾的决定（未开实盘却 `Demote`、已开却 `Promote`）会被框架忽略并
    /// 记一条 warn —— 不静默丢弃，便于发现判据写错。
    fn decide(&mut self, view: &SymbolPerformance<'_>) -> Decision;
}

/// 永不晋升的占位判据 —— 框架默认值。
///
/// 有意**不**内置任何"看起来合理"的统计规则：阈值的选择直接决定系统是在捕捉 edge 还是在
/// 挑噪声（见模块文档），这个决定必须由使用者显式作出，而不是继承一个默认值。
///
/// 用它启动时，模拟会照常运行与记录，只是永远不会拉起实盘 —— 适合先积累样本再定判据。
#[derive(Debug, Default)]
pub struct NeverPromote;

impl PromotionPolicy for NeverPromote {
    fn decide(&mut self, _view: &SymbolPerformance<'_>) -> Decision {
        Decision::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_promote_holds_regardless_of_profit() {
        let symbol = "BTC".to_string();
        let mut paper = SymbolRecord::new(symbol.clone(), 8);
        // 造一次盈利往返
        use crate::domain::{Exchange, Fill, FillReason, Side};
        let mk = |side: Side, price: f64, ts: Timestamp| Fill {
            exchange: Exchange::Binance,
            symbol: symbol.clone(),
            side,
            price,
            size: 1.0,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: ts,
            fee: 0.0,
            reason: FillReason::Normal,
        };
        paper.apply_fill(&mk(Side::Long, 100.0, 1));
        paper.apply_fill(&mk(Side::Short, 110.0, 2));
        assert!(paper.realized_pnl() > 0.0);

        let view = SymbolPerformance {
            symbol: &symbol,
            paper: &paper,
            live: None,
            live_since: None,
            now: 100,
        };
        assert_eq!(NeverPromote.decide(&view), Decision::Hold);
    }

    /// 判据是**有状态**的：框架允许它攒自己的状态（例如样本外确认的冷却期）。
    /// 这条测试固化该契约 —— 同一 symbol 的连续调用会看到判据自己的累积。
    #[test]
    fn policy_may_accumulate_its_own_state_across_calls() {
        struct CountingPolicy {
            calls: u32,
        }
        impl PromotionPolicy for CountingPolicy {
            fn decide(&mut self, _view: &SymbolPerformance<'_>) -> Decision {
                self.calls += 1;
                // 第三次调用才晋升 —— 模拟"需要累积到一定观察次数"
                if self.calls >= 3 {
                    Decision::Promote
                } else {
                    Decision::Hold
                }
            }
        }

        let symbol = "BTC".to_string();
        let paper = SymbolRecord::new(symbol.clone(), 8);
        let mut policy = CountingPolicy { calls: 0 };
        let view = || SymbolPerformance {
            symbol: &symbol,
            paper: &paper,
            live: None,
            live_since: None,
            now: 0,
        };
        assert_eq!(policy.decide(&view()), Decision::Hold);
        assert_eq!(policy.decide(&view()), Decision::Hold);
        assert_eq!(policy.decide(&view()), Decision::Promote);
    }

    /// live 已开启时，view 能给出运行时长 —— 降级判据通常需要它（例如"至少跑够 N 分钟再评"）
    #[test]
    fn live_view_exposes_elapsed_time_and_record() {
        let symbol = "BTC".to_string();
        let paper = SymbolRecord::new(symbol.clone(), 8);
        let live = SymbolRecord::new(symbol.clone(), 8);
        let view = SymbolPerformance {
            symbol: &symbol,
            paper: &paper,
            live: Some(&live),
            live_since: Some(1_000),
            now: 61_000,
        };
        assert!(view.is_live());
        assert_eq!(view.live_elapsed_ms(), 60_000);
        assert_eq!(view.live.map(|r| r.realized_pnl()), Some(0.0));
    }

    #[test]
    fn live_elapsed_is_zero_when_not_live() {
        let symbol = "BTC".to_string();
        let paper = SymbolRecord::new(symbol.clone(), 8);
        let view = SymbolPerformance {
            symbol: &symbol,
            paper: &paper,
            live: None,
            live_since: None,
            now: 1_000,
        };
        assert!(!view.is_live());
        assert_eq!(view.live_elapsed_ms(), 0);
    }
}

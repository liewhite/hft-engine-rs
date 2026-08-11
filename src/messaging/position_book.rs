//! `PositionBook` —— 只有持仓的账本：跨 symbol 的 [`SymbolPositions`] 容器。
//!
//! # 为什么单独有这个类型
//!
//! 持仓账本（`PositionLedgerActor`）此前持有整个 [`StateManager`]，而它一辈子只用得到
//! 持仓那一份：行情缓存、挂单跟踪、账户净值、希腊值它一个都不碰。这既是接口过宽
//! （`docs/architecture.md` 原则 P3），也让"三份持仓折叠"里它那一份看起来像是重复
//! 造出来的 —— 实际上它是**唯一被 REST 持续校验的那份**，理应是最瘦、最专一的。
//!
//! 拆出本类型之后：账本的字段类型就是它的职责声明，编译器保证它碰不到别的东西。
//!
//! # 防双计规则不在这里
//!
//! 「seed 之后、快照请求时刻之前送达的 Fill 要丢弃」这条规则安在
//! [`SymbolPositions::apply_fill`] 上，与 `seeded_at` 同一个结构。本类型只做按 symbol 的
//! 分发与注册范围管理 —— 规则只能有一份，否则账本与 executor 会各自演化出细微差别，
//! 而那种差别的表现形式是"对账偶尔误报漂移"，最难查。

use crate::domain::{Exchange, Position, Symbol};
use crate::messaging::state_manager::PositionBaseline;
use crate::messaging::{AccountData, AccountEvent, SymbolPositions};
use std::collections::HashMap;

/// 跨 symbol 的持仓账本
#[derive(Debug, Clone, Default)]
pub struct PositionBook {
    /// 已注册的 symbol -> 各所持仓。
    ///
    /// 键集兼作"本实例负责哪些 symbol"的判据，不另存副本：分桶部署下多实例共用同一账户，
    /// 交易所读数是**账户级全量**、含其他桶的 symbol，靠这个判据过滤。
    symbols: HashMap<Symbol, SymbolPositions>,
}

impl PositionBook {
    /// 追加注册要跟踪的 symbol（已存在的保持原状态不动）
    pub fn register_symbols(&mut self, symbols: &[Symbol]) {
        for symbol in symbols {
            self.symbols.entry(symbol.clone()).or_default();
        }
    }

    /// 写入一批持仓基线（投产握手，见 [`PositionBaseline`]）。
    ///
    /// symbol 未注册的基线跳过并记 error（基线属于策略订阅范围内的 symbol，范围外到达
    /// 说明调用方拼装错了）；已 seed 的 (所, symbol) 静默跳过（再晋升时的正常形态）。
    pub fn seed(&mut self, baselines: &[PositionBaseline]) {
        for baseline in baselines {
            let symbol = &baseline.position.symbol;
            let Some(positions) = self.symbols.get_mut(symbol) else {
                tracing::error!(
                    %symbol,
                    exchange = %baseline.position.exchange,
                    "seed: symbol 未在 PositionBook 注册，基线被跳过"
                );
                continue;
            };
            positions.seed(symbol, &baseline.position, baseline.snapshot_req_ts);
        }
    }

    /// 该 symbol 是否在跟踪范围内
    pub fn is_tracked(&self, symbol: &Symbol) -> bool {
        self.symbols.contains_key(symbol)
    }

    /// 喂入一条账户事件。只有**实盘的 Fill** 会改变账本 —— 通道 A 的增量输入就它一个；
    /// 模拟账户的成交绝不能进实盘持仓账本。
    ///
    /// 跟踪范围外的 symbol 静默丢弃：分桶部署下账户级私有流必然带来其他桶的成交，
    /// 那不是错误（该判断与 `Reconciler` 此前的 `is_tracked` 过滤同义，只是搬进了本类型）。
    pub fn apply(&mut self, event: &AccountEvent) {
        if event.account != crate::domain::AccountId::Live {
            return;
        }
        let AccountData::Fill(fill) = &event.data else {
            return;
        };
        let Some(positions) = self.symbols.get_mut(&fill.symbol) else {
            return;
        };
        positions.apply_fill(&fill.symbol, fill, event.local_ts);
    }

    /// 遍历已注册 symbol 的持仓投影
    pub fn iter(&self) -> impl Iterator<Item = (&Symbol, &SymbolPositions)> {
        self.symbols.iter()
    }

    /// 某 symbol 的持仓投影（未注册返回 `None`）
    pub fn get(&self, symbol: &Symbol) -> Option<&SymbolPositions> {
        self.symbols.get(symbol)
    }

    /// 全部**基线已写入**的腿（含 `size == 0` 的空仓腿）。
    ///
    /// 未 seed 的腿不在结果里 —— 那是"未知"而非"空仓"，见
    /// [`SymbolPositions::seeded_positions`]。对外快照（`GetLivePositions`）用的就是它。
    pub fn seeded_positions(&self) -> impl Iterator<Item = &Position> {
        self.symbols.values().flat_map(|p| p.seeded_positions())
    }

    /// 某 (所, symbol) 的仓位大小；未注册视为 0
    pub fn position_size(&self, exchange: Exchange, symbol: &Symbol) -> f64 {
        self.symbols
            .get(symbol)
            .map(|p| p.position_size(exchange))
            .unwrap_or(0.0)
    }
}

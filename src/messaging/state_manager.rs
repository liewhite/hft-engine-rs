use crate::domain::{Exchange, Greeks, MarketStatus, Order, Symbol, Timestamp, USDT};
use crate::domain::AccountInfo;
use crate::messaging::{ExchangeEventData, IncomeEvent, SymbolState};
use std::collections::HashMap;

/// 状态管理器 - 管理所有交易状态
pub struct StateManager {
    /// Per-symbol 状态
    states: HashMap<Symbol, SymbolState>,
    /// 全局 USDT 余额 (per exchange)
    balances: HashMap<Exchange, f64>,
    /// 账户信息: 净值 + 总持仓名义价值 (per exchange)
    account_infos: HashMap<Exchange, AccountInfo>,
    /// 账户希腊值 (per exchange, per ccy)
    greeks: HashMap<(Exchange, String), Greeks>,
    /// 币种现金余额 (per exchange, per ccy) — 用于修正 greeks delta
    cash_balances: HashMap<(Exchange, String), f64>,
    /// 交易所市场状态 (per exchange)
    market_statuses: HashMap<Exchange, MarketStatus>,
    /// 订单超时时间 (毫秒)
    order_timeout_ms: u64,
}

impl StateManager {
    /// 创建状态管理器
    pub fn new(symbols: &[Symbol], order_timeout_ms: u64) -> Self {
        let mut states = HashMap::new();
        for symbol in symbols {
            states.insert(symbol.clone(), SymbolState::new(symbol.clone()));
        }

        Self {
            states,
            balances: HashMap::new(),
            account_infos: HashMap::new(),
            greeks: HashMap::new(),
            cash_balances: HashMap::new(),
            market_statuses: HashMap::new(),
            order_timeout_ms,
        }
    }

    /// 追加注册需要跟踪的 symbol（已存在的保持原状态不动）
    ///
    /// 用于"symbol 集合在构造之后才确定"的消费者（如指标 actor：它订阅全量事件流，
    /// 但要跟踪哪些 symbol 由上层策略集合决定）。
    pub fn register_symbols(&mut self, symbols: &[Symbol]) {
        for symbol in symbols {
            self.states
                .entry(symbol.clone())
                .or_insert_with(|| SymbolState::new(symbol.clone()));
        }
    }

    // ==================== 下单接口 ====================

    /// 添加 pending order (由 StrategyRunner 调用，client_order_id 已生成)
    ///
    /// `created_at` 由调用方注入 (实盘传接收时刻、回测传虚拟事件时刻)，避免内部读墙钟破坏
    /// 回测确定性。
    ///
    /// symbol 未注册（策略对未订阅 symbol 下单）时记录 error 并跳过跟踪，不 panic。
    pub fn add_pending_order(&mut self, order: Order, created_at: Timestamp) {
        let symbol = order.symbol.clone();
        let Some(state) = self.states.get_mut(&symbol) else {
            tracing::error!(
                symbol = %symbol,
                "add_pending_order: symbol 未在 StateManager 注册，无法跟踪该订单"
            );
            return;
        };
        state.add_pending_order(order, created_at);
    }

    // ==================== 状态查询 ====================

    /// 获取指定 symbol 的状态
    pub fn symbol_state(&self, symbol: &Symbol) -> Option<&SymbolState> {
        self.states.get(symbol)
    }

    /// 遍历所有已注册 symbol 的状态（供观测层汇总）
    pub fn symbol_states(&self) -> impl Iterator<Item = (&Symbol, &SymbolState)> {
        self.states.iter()
    }

    /// 遍历所有已收到账户信息的交易所（供观测层汇总）
    pub fn account_infos(&self) -> impl Iterator<Item = (Exchange, &AccountInfo)> {
        self.account_infos.iter().map(|(e, i)| (*e, i))
    }

    /// 获取指定交易所的 USDT 余额
    ///
    /// 返回 None 表示该交易所的余额数据尚未到达
    pub fn usdt_balance(&self, exchange: Exchange) -> Option<f64> {
        self.balances.get(&exchange).copied()
    }

    /// 获取所有交易所的 USDT 总余额（仅包含已收到数据的交易所）
    pub fn total_usdt_balance(&self) -> f64 {
        self.balances.values().sum()
    }

    /// 获取指定交易所的账户信息 (equity + notional 原子性保证)
    ///
    /// 返回 None 表示该交易所的账户数据尚未到达
    pub fn account_info(&self, exchange: Exchange) -> Option<&AccountInfo> {
        self.account_infos.get(&exchange)
    }

    /// 获取指定交易所的账户净值
    ///
    /// 返回 None 表示该交易所的净值数据尚未到达
    pub fn equity(&self, exchange: Exchange) -> Option<f64> {
        self.account_infos.get(&exchange).map(|i| i.equity)
    }

    /// 获取所有交易所的总净值（仅包含已收到数据的交易所）
    pub fn total_equity(&self) -> f64 {
        self.account_infos.values().map(|i| i.equity).sum()
    }

    /// 获取指定交易所的账户总持仓名义价值
    ///
    /// 返回 None 表示该交易所的名义价值数据尚未到达
    pub fn account_notional(&self, exchange: Exchange) -> Option<f64> {
        self.account_infos.get(&exchange).map(|i| i.notional)
    }

    /// 获取所有交易所的总持仓名义价值（仅包含已收到数据的交易所）
    pub fn total_account_notional(&self) -> f64 {
        self.account_infos.values().map(|i| i.notional).sum()
    }

    /// 获取指定交易所和币种的希腊值 (delta 已包含现货余额修正)
    ///
    /// 仅当 greeks 推送和 cashBal 均已到达时才返回，避免未修正的 delta 引发策略误判
    pub fn greeks(&self, exchange: Exchange, ccy: &str) -> Option<Greeks> {
        let key = (exchange, ccy.to_string());
        let g = self.greeks.get(&key)?;
        let &cash_bal = self.cash_balances.get(&key)?;
        let mut corrected = g.clone();
        corrected.delta += cash_bal;
        Some(corrected)
    }

    /// 获取指定交易所的市场状态（默认 Closed，安全侧）
    pub fn market_status(&self, exchange: Exchange) -> MarketStatus {
        self.market_statuses
            .get(&exchange)
            .copied()
            .unwrap_or(MarketStatus::Closed)
    }

    /// 检查指定 symbol 是否有未完成订单
    pub fn has_pending_orders(&self, symbol: &Symbol) -> bool {
        self.states
            .get(symbol)
            .map(|s| s.has_pending_orders())
            .unwrap_or(false)
    }

    // ==================== 事件处理 ====================

    /// 处理事件，更新状态
    pub fn apply(&mut self, event: &IncomeEvent) {
        match &event.data {
            // 全局事件: Balance
            ExchangeEventData::Balance(balance) => {
                if balance.asset == USDT {
                    tracing::debug!(
                        exchange = %balance.exchange,
                        available = balance.available,
                        "USDT balance updated"
                    );
                    self.balances.insert(balance.exchange, balance.available);
                }
                // 存储币种现金余额 (用于修正 greeks delta)
                self.cash_balances.insert(
                    (balance.exchange, balance.asset.clone()),
                    balance.available,
                );
            }
            // 全局事件: AccountInfo (equity + notional 原子写入)
            ExchangeEventData::AccountInfo {
                exchange,
                equity,
                notional,
            } => {
                tracing::debug!(
                    exchange = %exchange,
                    equity = equity,
                    notional = notional,
                    "AccountInfo updated"
                );
                self.account_infos.insert(*exchange, AccountInfo {
                    equity: *equity,
                    notional: *notional,
                });
            }
            // 全局事件: Greeks
            ExchangeEventData::Greeks(g) => {
                self.greeks.insert((g.exchange, g.ccy.clone()), g.clone());
            }
            // 全局事件: ExchangeStatus
            ExchangeEventData::ExchangeStatus { exchange, status } => {
                self.market_statuses.insert(*exchange, *status);
            }
            // 全局事件: 券源/汇率读数——策略经 on_borrow_fee/on_exchange_rate 自持，
            // StateManager 不缓存 (最小改动)；显式空处理，避免落入 symbol 路由分支。
            ExchangeEventData::BorrowFee(_) | ExchangeEventData::ExchangeRate(_) => {}
            // 全局事件: 持仓对账读数 —— 只服务 `PositionReconcileActor`，不进任何本地状态。
            // 写进来就等于恢复了 `SymbolState` 明确否掉的"用快照覆写持仓"
            // （会与 Fill 流重复计算同一笔成交）。
            ExchangeEventData::PositionReport { .. } => {}
            // 全局事件: Clock (检查订单超时)
            ExchangeEventData::Clock => {
                let now = event.local_ts;
                for state in self.states.values_mut() {
                    state.remove_timed_out_orders(now, self.order_timeout_ms);
                }
            }
            // Symbol 事件: 委托对应 SymbolState 处理
            // 事件由 IncomeProcessorActor 按 (exchange, symbol) 路由，正常只有已注册 symbol 会到达。
            // 若 symbol 缺失或无对应状态，说明路由逻辑有 bug，记录 error 后忽略（不 panic）。
            _ => {
                let Some(symbol) = event.symbol() else {
                    tracing::error!("symbol 事件缺少 symbol（路由 bug），忽略");
                    return;
                };
                let Some(state) = self.states.get_mut(symbol) else {
                    tracing::error!(symbol = %symbol, "StateManager 无此 symbol 状态（路由 bug），忽略事件");
                    return;
                };
                state.apply(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Position;
    use crate::messaging::ExchangeEventData;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn ev(data: ExchangeEventData) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 1,
            local_ts: 1,
            data,
        }
    }

    fn position(size: f64) -> Position {
        Position {
            exchange: EX,
            symbol: SYM.to_string(),
            size,
            entry_price: 100.0,
            unrealized_pnl: 0.0,
        }
    }

    /// 对账读数只供 `PositionReconcileActor` 比对，**绝不**写入本地持仓。
    ///
    /// 若写入，就等于恢复了"用快照覆写持仓"——而快照与 Fill 流叠加会重复计算同一笔成交，
    /// 这正是 `PositionBaseline` 只允许出现一次的原因。
    #[test]
    fn position_report_does_not_touch_local_position() {
        let mut manager = StateManager::new(&[SYM.to_string()], 0);
        manager.apply(&ev(ExchangeEventData::PositionBaseline(position(2.0))));

        // 交易所报了个不一样的值（这正是"漂移"的样子）——本地持仓不动，由对账层去告警
        manager.apply(&ev(ExchangeEventData::PositionReport {
            exchange: EX,
            positions: vec![position(99.0)],
        }));

        let state = manager.symbol_state(&SYM.to_string()).expect("state");
        assert_eq!(state.position_size(EX), 2.0);
    }
}

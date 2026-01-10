use crate::domain::{now_ms, Exchange, Symbol, USDT};
use crate::messaging::{ExchangeEventData, IncomeEvent, SymbolState};
use std::collections::HashMap;

/// 状态管理器 - 管理所有交易状态
pub struct StateManager {
    /// Per-symbol 状态
    states: HashMap<Symbol, SymbolState>,
    /// 全局 USDT 余额 (per exchange)
    balances: HashMap<Exchange, f64>,
    /// 账户净值 (per exchange)
    equities: HashMap<Exchange, f64>,
    /// 账户总持仓名义价值 (per exchange)
    account_notionals: HashMap<Exchange, f64>,
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
            equities: HashMap::new(),
            account_notionals: HashMap::new(),
            order_timeout_ms,
        }
    }

    // ==================== 下单接口 ====================

    /// 添加 pending order (由 Executor 调用，client_order_id 已生成)
    ///
    /// # Panics
    /// symbol 不存在时 panic（表示配置错误）
    pub fn add_pending_order(
        &mut self,
        symbol: &Symbol,
        client_order_id: String,
        exchange: Exchange,
    ) {
        self.states
            .get_mut(symbol)
            .expect("Symbol not found in StateManager")
            .add_pending_order(client_order_id, exchange, now_ms());
    }

    // ==================== 状态查询 ====================

    /// 获取指定 symbol 的状态
    pub fn symbol_state(&self, symbol: &Symbol) -> Option<&SymbolState> {
        self.states.get(symbol)
    }

    /// 获取指定交易所的 USDT 余额
    pub fn usdt_balance(&self, exchange: Exchange) -> f64 {
        self.balances.get(&exchange).copied().unwrap_or(0.0)
    }

    /// 获取所有交易所的 USDT 总余额
    pub fn total_usdt_balance(&self) -> f64 {
        self.balances.values().sum()
    }

    /// 获取指定交易所的账户净值
    pub fn equity(&self, exchange: Exchange) -> f64 {
        self.equities.get(&exchange).copied().unwrap_or(0.0)
    }

    /// 获取所有交易所的总净值
    pub fn total_equity(&self) -> f64 {
        self.equities.values().sum()
    }

    /// 获取指定交易所的账户总持仓名义价值
    pub fn account_notional(&self, exchange: Exchange) -> f64 {
        self.account_notionals.get(&exchange).copied().unwrap_or(0.0)
    }

    /// 获取所有交易所的总持仓名义价值
    pub fn total_account_notional(&self) -> f64 {
        self.account_notionals.values().sum()
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
            }
            // 全局事件: AccountInfo (equity + notional)
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
                self.equities.insert(*exchange, *equity);
                self.account_notionals.insert(*exchange, *notional);
            }
            // 全局事件: Clock (检查订单超时)
            ExchangeEventData::Clock => {
                let now = event.local_ts;
                for state in self.states.values_mut() {
                    state.remove_timed_out_orders(now, self.order_timeout_ms);
                }
            }
            // Symbol 事件: 委托对应 SymbolState 处理
            _ => {
                let symbol = event
                    .symbol()
                    .expect("Symbol event must have symbol");
                self.states
                    .get_mut(symbol)
                    .expect("Symbol not found in StateManager")
                    .apply(event);
            }
        }
    }
}

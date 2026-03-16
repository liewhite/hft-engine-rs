use crate::domain::{now_ms, Exchange, Greeks, MarketStatus, Order, Symbol, USDT};
use crate::exchange::AccountInfo;
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

    // ==================== 下单接口 ====================

    /// 添加 pending order (由 Executor 调用，client_order_id 已生成)
    ///
    /// # Panics
    /// symbol 不存在时 panic（表示配置错误）
    pub fn add_pending_order(&mut self, order: Order) {
        let symbol = order.symbol.clone();
        self.states
            .get_mut(&symbol)
            .expect("Symbol not found in StateManager")
            .add_pending_order(order, now_ms());
    }

    // ==================== 状态查询 ====================

    /// 获取指定 symbol 的状态
    pub fn symbol_state(&self, symbol: &Symbol) -> Option<&SymbolState> {
        self.states.get(symbol)
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
    pub fn greeks(&self, exchange: Exchange, ccy: &str) -> Option<Greeks> {
        let key = (exchange, ccy.to_string());
        self.greeks.get(&key).map(|g| {
            let mut corrected = g.clone();
            if let Some(&cash_bal) = self.cash_balances.get(&key) {
                corrected.delta += cash_bal;
            }
            corrected
        })
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
            // 全局事件: Clock (检查订单超时)
            ExchangeEventData::Clock => {
                let now = event.local_ts;
                for state in self.states.values_mut() {
                    state.remove_timed_out_orders(now, self.order_timeout_ms);
                }
            }
            // Symbol 事件: 委托对应 SymbolState 处理
            // 事件由 IncomeProcessorActor 按 (exchange, symbol) 路由，只有已注册的 symbol 才会到达，
            // 因此 symbol 查找不会失败（如果失败说明路由逻辑有 bug，应立即暴露）
            _ => {
                let symbol = event
                    .symbol()
                    .expect("Symbol event must have symbol");
                self.states
                    .get_mut(symbol)
                    .expect("Symbol not found in StateManager: routing bug")
                    .apply(event);
            }
        }
    }
}

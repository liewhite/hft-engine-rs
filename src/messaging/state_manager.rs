use crate::domain::{Exchange, Greeks, MarketStatus, Order, Position, Symbol, Timestamp, USDT};
use crate::domain::AccountInfo;
use crate::messaging::{AccountData, IncomeEvent, MarketData, SymbolState};
use std::collections::HashMap;

/// 一份持仓基线：某 (所, symbol) 的起始持仓 + 其 REST 快照的**请求**时刻。
///
/// 这是**投产握手的载荷，不是事件** —— 它只属于特定消费者（executor 出生时随
/// `ExecutorArgs`、镜像随 `RegisterSymbols`），不上广播总线。生产者只有一个：
/// `ManagerActor` 投产期的一次 REST 快照，同一份快照喂给全部账本消费者
/// （executor / 对账镜像 / 观测镜像），三者口径一致。
///
/// `snapshot_req_ts` 的用途见 [`SymbolState::seed_position`]：seed 之后、该时刻
/// 之前送达的 Fill 已含在快照里，丢弃避免双计。
#[derive(Debug, Clone)]
pub struct PositionBaseline {
    pub position: Position,
    pub snapshot_req_ts: Timestamp,
}

/// **账户**投影：余额 / 净值 / 希腊值，全部 per exchange。
///
/// 与 per-symbol 的三个投影（见 [`SymbolState`]）并列的第四个 —— 它是账户级的，
/// 不按 symbol 索引，所以住在 [`StateManager`] 上而不在 `SymbolState` 里。
#[derive(Debug, Clone, Default)]
pub struct AccountView {
    /// 全局 USDT 余额 (per exchange)
    balances: HashMap<Exchange, f64>,
    /// 账户信息: 净值 + 总持仓名义价值 (per exchange)
    account_infos: HashMap<Exchange, AccountInfo>,
    /// 账户希腊值 (per exchange, per ccy)
    greeks: HashMap<(Exchange, String), Greeks>,
    /// 币种现金余额 (per exchange, per ccy) — 用于修正 greeks delta
    cash_balances: HashMap<(Exchange, String), f64>,
}

impl AccountView {
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
}

/// 状态管理器 - 四个投影的组合容器
pub struct StateManager {
    /// Per-symbol 状态（内部又分行情 / 持仓 / 挂单三个投影，见 [`SymbolState`]）
    states: HashMap<Symbol, SymbolState>,
    /// 账户投影
    account: AccountView,
    /// 交易所市场状态 (per exchange)。
    ///
    /// 公共行情，不属于 [`AccountView`]；又是账户级而非 per-symbol，故留在本层。
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
            account: AccountView::default(),
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

    /// 写入一批持仓基线（投产握手，见 [`PositionBaseline`]）。
    ///
    /// symbol 未注册的基线直接跳过并记 error（基线属于策略订阅范围内的 symbol，
    /// 范围外到达说明调用方拼装错了）；已 seed 的 (所, symbol) 静默跳过
    /// （再晋升时的正常形态，见 [`SymbolState::seed_position`]）。
    pub fn seed_positions(&mut self, baselines: &[PositionBaseline]) {
        for baseline in baselines {
            let symbol = &baseline.position.symbol;
            let Some(state) = self.states.get_mut(symbol) else {
                tracing::error!(
                    %symbol,
                    exchange = %baseline.position.exchange,
                    "seed_positions: symbol 未在 StateManager 注册，基线被跳过"
                );
                continue;
            };
            state.seed_position(&baseline.position, baseline.snapshot_req_ts);
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

    /// 账户投影（只读）—— 只需净值 / 余额 / 希腊值的消费者可以只拿这一份
    pub fn account_view(&self) -> &AccountView {
        &self.account
    }

    /// 遍历全部 symbol 中**基线已写入**的持仓腿（见 [`SymbolState::seeded_positions`]）。
    ///
    /// 供持仓账本对外应答快照查询：未 seed 的腿是「未知」，不出现在结果里。
    pub fn seeded_positions(&self) -> impl Iterator<Item = &Position> {
        self.states.values().flat_map(|s| s.seeded_positions())
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
        match event {
            IncomeEvent::Market(m) => match &m.data {
                // 全局事件: ExchangeStatus
                MarketData::ExchangeStatus { exchange, status } => {
                    self.market_statuses.insert(*exchange, *status);
                }
                // 全局事件: 券源/汇率读数 —— 策略在 on_event 里自行消费并自持，
                // StateManager 不缓存；显式空处理，避免落入 symbol 路由分支。
                MarketData::BorrowFee(_) | MarketData::ExchangeRate(_) => {}
                // 自定义事件：框架只运送不理解，策略在 on_event 里自行消费并自持。
                // 必须显式空处理 —— 无 scope 的自定义事件会被兜底分支误报"缺少 symbol 的路由 bug"。
                MarketData::Custom(_) => {}
                // 全局事件: Clock (检查订单超时)
                MarketData::Clock => {
                    let now = m.local_ts;
                    for state in self.states.values_mut() {
                        state.remove_timed_out_orders(now, self.order_timeout_ms);
                    }
                }
                // Symbol 行情: 委托对应 SymbolState 处理
                _ => self.route_to_symbol_state(event),
            },
            IncomeEvent::Account(a) => match &a.data {
                // 账户级事件: Balance
                AccountData::Balance(balance) => {
                    if balance.asset == USDT {
                        tracing::debug!(
                            exchange = %balance.exchange,
                            available = balance.available,
                            "USDT balance updated"
                        );
                        self.account.balances.insert(balance.exchange, balance.available);
                    }
                    // 存储币种现金余额 (用于修正 greeks delta)
                    self.account.cash_balances.insert(
                        (balance.exchange, balance.asset.clone()),
                        balance.available,
                    );
                }
                // 账户级事件: AccountInfo (equity + notional 原子写入)
                AccountData::AccountInfo {
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
                    self.account.account_infos.insert(*exchange, AccountInfo {
                        equity: *equity,
                        notional: *notional,
                    });
                }
                // 账户级事件: Greeks
                AccountData::Greeks(g) => {
                    self.account.greeks.insert((g.exchange, g.ccy.clone()), g.clone());
                }
                // Symbol 私有事件 (OrderUpdate/Fill/FundingFee): 委托对应 SymbolState
                _ => self.route_to_symbol_state(event),
            },
        }
    }

    /// 按 symbol 路由到对应 SymbolState。
    /// 事件由 IncomeProcessorActor 按 (exchange, symbol) 路由，正常只有已注册 symbol 会到达。
    /// 若 symbol 缺失或无对应状态，说明路由逻辑有 bug，记录 error 后忽略（不 panic）。
    fn route_to_symbol_state(&mut self, event: &IncomeEvent) {
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

    // ==================== 账户投影的委托（保持既有调用面不变） ====================

    pub fn account_infos(&self) -> impl Iterator<Item = (Exchange, &AccountInfo)> {
        self.account.account_infos()
    }

    pub fn usdt_balance(&self, exchange: Exchange) -> Option<f64> {
        self.account.usdt_balance(exchange)
    }

    pub fn total_usdt_balance(&self) -> f64 {
        self.account.total_usdt_balance()
    }

    pub fn account_info(&self, exchange: Exchange) -> Option<&AccountInfo> {
        self.account.account_info(exchange)
    }

    pub fn equity(&self, exchange: Exchange) -> Option<f64> {
        self.account.equity(exchange)
    }

    pub fn total_equity(&self) -> f64 {
        self.account.total_equity()
    }

    pub fn account_notional(&self, exchange: Exchange) -> Option<f64> {
        self.account.account_notional(exchange)
    }

    pub fn total_account_notional(&self) -> f64 {
        self.account.total_account_notional()
    }

    pub fn greeks(&self, exchange: Exchange, ccy: &str) -> Option<Greeks> {
        self.account.greeks(exchange, ccy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fill, FillReason, Side};

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn position(size: f64) -> Position {
        Position {
            exchange: EX,
            symbol: SYM.to_string(),
            size,
        }
    }

    fn baseline(size: f64, snapshot_req_ts: Timestamp) -> PositionBaseline {
        PositionBaseline {
            position: position(size),
            snapshot_req_ts,
        }
    }

    fn fill_at(size: f64, local_ts: Timestamp) -> IncomeEvent {
        IncomeEvent::account(
            crate::domain::AccountId::Live,
            local_ts,
            local_ts,
            AccountData::Fill(Fill {
                exchange: EX,
                symbol: SYM.to_string(),
                side: Side::Long,
                price: 100.0,
                size,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: local_ts,
                fee: 0.0,
                reason: FillReason::Normal,
            }),
        )
    }

    /// 基线只写一次：第二次 seed 静默跳过（再晋升时的正常形态），存量不被覆写
    #[test]
    fn seed_is_idempotent_and_never_overwrites() {
        let mut manager = StateManager::new(&[SYM.to_string()], 0);
        manager.seed_positions(&[baseline(2.0, 10)]);
        manager.seed_positions(&[baseline(99.0, 20)]);

        let state = manager.symbol_state(&SYM.to_string()).expect("state");
        assert_eq!(state.position_size(EX), 2.0, "重复 seed 不得覆写存量");
    }

    /// **防双计**：快照请求时刻之前送达的 Fill 已含在快照里，seed 之后到达必须丢弃；
    /// 之后送达的照常累加。这条规则对 executor / 对账镜像 / 观测镜像同一份。
    #[test]
    fn fills_covered_by_snapshot_are_dropped_after_seed() {
        let mut manager = StateManager::new(&[SYM.to_string()], 0);
        // 快照请求时刻 t=10，读到存量 2.0
        manager.seed_positions(&[baseline(2.0, 10)]);

        // t=5 送达的 Fill（成交已含在快照里）迟到进入 —— 丢弃
        manager.apply(&fill_at(1.0, 5));
        // t=15 送达的 Fill 是快照之后的新成交 —— 累加
        manager.apply(&fill_at(1.0, 15));

        let state = manager.symbol_state(&SYM.to_string()).expect("state");
        assert_eq!(state.position_size(EX), 3.0, "应为 2.0(快照) + 1.0(新成交)");
    }

    /// 未 seed 的所从 0 起算（模拟账户、未配置凭证的所）：Fill 直接累加，不受过滤影响
    #[test]
    fn unseeded_exchange_accumulates_from_zero() {
        let mut manager = StateManager::new(&[SYM.to_string()], 0);
        manager.apply(&fill_at(1.5, 5));
        let state = manager.symbol_state(&SYM.to_string()).expect("state");
        assert_eq!(state.position_size(EX), 1.5);
    }

    /// 未注册 symbol 的基线被跳过（不 panic、不隐式建状态）
    #[test]
    fn baseline_for_unregistered_symbol_is_skipped() {
        let mut manager = StateManager::new(&[], 0);
        manager.seed_positions(&[baseline(2.0, 10)]);
        assert!(manager.symbol_state(&SYM.to_string()).is_none());
    }
}

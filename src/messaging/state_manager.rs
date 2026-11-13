use crate::domain::AccountInfo;
use crate::domain::{Exchange, Greeks, MarketStatus, Order, Position, Symbol, Timestamp, USDT};
use crate::messaging::{
    AccountData, ExchangeAccountData, IncomeEvent, LegValue, MarketData, SymbolState,
};
use std::collections::HashMap;

/// 一份持仓基线：某 (所, symbol) 的起始持仓 + 其 REST 快照的**请求**时刻。
///
/// 这是**投产握手的载荷，不是事件** —— 它只属于特定消费者（executor 出生时随
/// `ExecutorArgs`、镜像随 `RegisterSymbols`），不上广播总线。生产者只有一个：
/// manager 投产期的一次 REST 快照，同一份快照喂给全部账本消费者
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
    /// 本地持仓账本给出的各币持仓读数 (per exchange)，见 [`crate::messaging::PositionValues`]
    position_values: HashMap<Exchange, HashMap<Symbol, LegValue>>,
}

impl AccountView {
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

    /// 本地持仓账本里该所各币的持仓读数（数量 + 估值价）。
    ///
    /// 返回 None 表示账本还没发过该所的读数（价值未知，不是 0）。
    pub fn position_values(&self, exchange: Exchange) -> Option<&HashMap<Symbol, LegValue>> {
        self.position_values.get(&exchange)
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
}

impl StateManager {
    /// 创建状态管理器
    pub fn new(symbols: &[Symbol]) -> Self {
        let mut states = HashMap::new();
        for symbol in symbols {
            states.insert(symbol.clone(), SymbolState::new(symbol.clone()));
        }

        Self {
            states,
            account: AccountView::default(),
            market_statuses: HashMap::new(),
        }
    }

    /// 写入一批持仓基线（投产握手，见 [`PositionBaseline`]）。
    ///
    /// symbol 未注册的基线直接跳过并记 error（基线属于策略订阅范围内的 symbol，
    /// 范围外到达说明调用方拼装错了）。只投产一次，每个 (所, symbol) 只 seed 一次。
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

    /// 登记一张刚发出的单（由 StrategyRunner 调用：已过下单范围闸门、已赋号取整）
    ///
    /// `created_at` 由调用方注入 (实盘传接收时刻、回测传虚拟事件时刻)，避免内部读墙钟破坏
    /// 回测确定性。
    pub fn add_pending_order(&mut self, order: Order, created_at: Timestamp) {
        self.states
            .get_mut(&order.symbol)
            .expect("下单范围闸门只放行订阅范围内的币，而它们在构造时都已注册")
            .add_pending_order(order, created_at);
    }

    /// 策略要撤这张单：记下「撤单已发」，`Ok` 带这张单所在的所、表示该发（见
    /// [`crate::messaging::SymbolOrders::mark_cancel_sent`]）。调用方已确认币在范围内
    pub fn mark_cancel_sent(
        &mut self,
        symbol: &Symbol,
        client_order_id: &str,
        now: Timestamp,
    ) -> Result<Exchange, &'static str> {
        self.states
            .get_mut(symbol)
            .expect("调用方已确认币在订阅范围内（已注册）")
            .mark_cancel_sent(client_order_id, now)
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

    /// 账户投影（只读）。
    ///
    /// 账户级读数**只有这一条路**可达 —— 此前 `StateManager` 上还挂着九个同名转发方法，
    /// 于是同一个事实有两个入口，两边的文档与返回语义得各维护一遍（原则 P1）。
    /// 消费者要么只拿这一份投影，要么在其上再包一层裁剪（见
    /// [`crate::strategy::StrategyView`]）。
    pub fn account_view(&self) -> &AccountView {
        &self.account
    }

    /// 获取指定交易所的市场状态（默认 Closed，安全侧）
    pub fn market_status(&self, exchange: Exchange) -> MarketStatus {
        self.market_statuses
            .get(&exchange)
            .copied()
            .unwrap_or(MarketStatus::Closed)
    }

    // ==================== 事件处理 ====================

    /// 处理事件，更新状态。`Err` = 有单「等交易所」太久（见 [`crate::messaging::ORDER_STUCK_MS`]），
    /// 跟踪表已说不清挂单与持仓，调用方应停机。
    ///
    /// 两层 match 都不带默认分支：新增一种事件时这里编译失败，必须明确写出它的处理
    /// （哪怕是空处理），不会悄悄落进某个兜底分支。
    pub fn apply(&mut self, event: &IncomeEvent) -> Result<(), String> {
        match event {
            IncomeEvent::Market(m) => match &m.data {
                // 全局事件: ExchangeStatus
                MarketData::ExchangeStatus { exchange, status } => {
                    self.market_statuses.insert(*exchange, *status);
                }
                // 券源/汇率读数 —— 策略在 on_event 里自行消费并自持，StateManager 不缓存。
                MarketData::BorrowFee(_) | MarketData::ExchangeRate(_) => {}
                // 自定义事件：框架只运送不理解，策略在 on_event 里自行消费并自持。
                MarketData::Custom(_) => {}
                // 全局事件: Clock (检查有没有单等交易所太久)
                MarketData::Clock => {
                    for state in self.states.values() {
                        state.check_stuck_orders(m.local_ts)?;
                    }
                }
                // 币级行情: 委托对应 SymbolState（各变体直接取自己的 symbol）
                MarketData::FundingRate(r) => self.route_market(&r.symbol, &m.data),
                MarketData::BBO(b) => self.route_market(&b.symbol, &m.data),
                MarketData::MarketTrade(t) => self.route_market(&t.symbol, &m.data),
                MarketData::MarkPrice(mp) => self.route_market(&mp.symbol, &m.data),
                MarketData::IndexPrice(ip) => self.route_market(&ip.symbol, &m.data),
                MarketData::Candle(c) => self.route_market(&c.symbol, &m.data),
                MarketData::HistoryCandles(h) => self.route_market(&h.symbol, &m.data),
            },
            IncomeEvent::Account(a) => match &a.data {
                AccountData::Symbol(data) => {
                    if let Some(state) = self.routed_state(data.symbol()) {
                        state.apply_account(data, a.local_ts);
                    }
                }
                AccountData::Exchange(data) => self.apply_exchange_account(data),
            },
        }
        Ok(())
    }

    fn route_market(&mut self, symbol: &Symbol, data: &MarketData) {
        if let Some(state) = self.routed_state(symbol) {
            state.apply_market(data);
        }
    }

    /// 所级账户事件写进账户投影
    fn apply_exchange_account(&mut self, data: &ExchangeAccountData) {
        match data {
            ExchangeAccountData::Balance(balance) => {
                if balance.asset == USDT {
                    tracing::debug!(
                        exchange = %balance.exchange,
                        cash = balance.cash,
                        "USDT balance updated"
                    );
                    self.account.balances.insert(balance.exchange, balance.cash);
                }
                // 存储币种现金余额 (用于修正 greeks delta)
                self.account
                    .cash_balances
                    .insert((balance.exchange, balance.asset.clone()), balance.cash);
            }
            // equity + notional 原子写入
            ExchangeAccountData::AccountInfo {
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
                self.account.account_infos.insert(
                    *exchange,
                    AccountInfo {
                        equity: *equity,
                        notional: *notional,
                    },
                );
            }
            ExchangeAccountData::Greeks(g) => {
                self.account
                    .greeks
                    .insert((g.exchange, g.ccy.clone()), g.clone());
            }
            ExchangeAccountData::PositionValues(v) => {
                self.account
                    .position_values
                    .insert(v.exchange, v.legs.clone());
            }
        }
    }

    /// 按 symbol 取对应 SymbolState。
    /// 分发层已按订阅范围过滤，正常只有已注册 symbol 会到达；没有对应状态说明路由有 bug，
    /// 记录 error 后忽略（不 panic）。
    fn routed_state(&mut self, symbol: &Symbol) -> Option<&mut SymbolState> {
        let state = self.states.get_mut(symbol);
        if state.is_none() {
            tracing::error!(symbol = %symbol, "StateManager 无此 symbol 状态（路由 bug），忽略事件");
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Fill, FillReason, Side};
    use crate::messaging::SymbolAccountData;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn position(size: Decimal) -> Position {
        Position {
            exchange: EX,
            symbol: SYM.to_string(),
            size,
        }
    }

    fn baseline(size: Decimal, snapshot_req_ts: Timestamp) -> PositionBaseline {
        PositionBaseline {
            position: position(size),
            snapshot_req_ts,
        }
    }

    fn fill_at(size: Decimal, local_ts: Timestamp) -> IncomeEvent {
        IncomeEvent::account(
            crate::domain::AccountId::Live,
            local_ts,
            local_ts,
            AccountData::Symbol(SymbolAccountData::Fill(Fill {
                exchange: EX,
                symbol: SYM.to_string(),
                side: Side::Long,
                price: dec!(100.0),
                size,
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: local_ts,
                fee: 0.0,
                reason: FillReason::Normal,
            })),
        )
    }

    /// **防双计**：快照请求时刻之前送达的 Fill 已含在快照里，seed 之后到达必须丢弃；
    /// 之后送达的照常累加。这条规则对 executor / 对账镜像 / 观测镜像同一份。
    #[test]
    fn fills_covered_by_snapshot_are_dropped_after_seed() {
        let mut manager = StateManager::new(&[SYM.to_string()]);
        // 快照请求时刻 t=10，读到存量 2.0
        manager.seed_positions(&[baseline(dec!(2.0), 10)]);

        // t=5 送达的 Fill（成交已含在快照里）迟到进入 —— 丢弃
        manager.apply(&fill_at(dec!(1.0), 5)).unwrap();
        // t=15 送达的 Fill 是快照之后的新成交 —— 累加
        manager.apply(&fill_at(dec!(1.0), 15)).unwrap();

        let state = manager.symbol_state(&SYM.to_string()).expect("state");
        assert_eq!(
            state.position_book().position_size(EX),
            dec!(3.0),
            "应为 2.0(快照) + 1.0(新成交)"
        );
    }

    /// 未 seed 的所从 0 起算（模拟账户、未配置凭证的所）：Fill 直接累加，不受过滤影响
    #[test]
    fn unseeded_exchange_accumulates_from_zero() {
        let mut manager = StateManager::new(&[SYM.to_string()]);
        manager.apply(&fill_at(dec!(1.5), 5)).unwrap();
        let state = manager.symbol_state(&SYM.to_string()).expect("state");
        assert_eq!(state.position_book().position_size(EX), dec!(1.5));
    }

    /// 未注册 symbol 的基线被跳过（不 panic、不隐式建状态）
    #[test]
    fn baseline_for_unregistered_symbol_is_skipped() {
        let mut manager = StateManager::new(&[]);
        manager.seed_positions(&[baseline(dec!(2.0), 10)]);
        assert!(manager.symbol_state(&SYM.to_string()).is_none());
    }
}

use crate::domain::{Exchange, Order, OrderType, Price, Quantity, Rate, Side, Symbol, TimeInForce, BBO};
use crate::messaging::{ExchangeEvent, SymbolState};
use crate::strategy::{MarketDataType, Signal, Strategy};
use std::collections::HashMap;
use uuid::Uuid;

/// 资金费率套利策略配置
#[derive(Debug, Clone)]
pub struct FundingArbConfig {
    /// 最小日化费率差 (开仓阈值)
    pub min_spread: Rate,
    /// 最大日化费率差 (限制风险)
    pub max_spread: Rate,
    /// 平仓日化费率差阈值
    pub close_spread: Rate,
    /// 单笔最大下单金额 (USDT)
    pub max_notional: f64,
    /// 最大持仓数量
    pub max_quantity: Quantity,
}

impl Default for FundingArbConfig {
    fn default() -> Self {
        Self {
            min_spread: 0.0005,   // 0.05%
            max_spread: 0.002,    // 0.2%
            close_spread: 0.0002, // 0.02%
            max_notional: 1000.0, // 1000 USDT
            max_quantity: 1.0,
        }
    }
}

/// 开仓条件检查结果
struct OpenCondition {
    short_ex: Exchange,
    short_bbo: BBO,
    long_ex: Exchange,
    long_bbo: BBO,
}

/// 资金费率套利策略 (单 symbol)
pub struct FundingArbStrategy {
    config: FundingArbConfig,
    exchanges: Vec<Exchange>,
    symbol: Symbol,
    state: SymbolState,
    /// 各交易所 USDT 可用余额 (用于风控)
    usdt_balances: HashMap<Exchange, f64>,
}

impl FundingArbStrategy {
    pub fn new(config: FundingArbConfig, exchanges: Vec<Exchange>, symbol: Symbol) -> Self {
        Self {
            config,
            exchanges,
            state: SymbolState::new(symbol.clone()),
            symbol,
            usdt_balances: HashMap::new(),
        }
    }

    /// 获取指定交易所的 USDT 可用余额
    pub fn usdt_balance(&self, exchange: Exchange) -> f64 {
        self.usdt_balances.get(&exchange).copied().unwrap_or(0.0)
    }

    /// 获取所有交易所的 USDT 总可用余额
    pub fn total_usdt_balance(&self) -> f64 {
        self.usdt_balances.values().sum()
    }

    /// 检查开仓条件 (静态方法，避免借用冲突)
    fn check_open_condition(
        state: &SymbolState,
        config: &FundingArbConfig,
    ) -> Option<OpenCondition> {
        let spread = state.funding_spread()?;

        if spread.abs() < config.min_spread {
            return None;
        }

        if spread.abs() > config.max_spread {
            tracing::warn!(
                symbol = %state.symbol,
                spread = spread,
                "Spread exceeds max threshold"
            );
            return None;
        }

        // 如果已有持仓，不开新仓
        if state.has_positions() {
            return None;
        }

        // 如果有未完成订单，等待
        if state.has_pending_orders() {
            return None;
        }

        let (short_ex, short_rate) = state.best_short_exchange()?;
        let (long_ex, long_rate) = state.best_long_exchange()?;

        // 检查两个交易所的 BBO 是否都存在
        let short_bbo = state.bbo(short_ex)?.clone();
        let long_bbo = state.bbo(long_ex)?.clone();

        tracing::info!(
            symbol = %state.symbol,
            spread = spread,
            short_exchange = %short_ex,
            short_rate = short_rate.rate,
            long_exchange = %long_ex,
            long_rate = long_rate.rate,
            "Opening condition met"
        );

        Some(OpenCondition {
            short_ex,
            short_bbo,
            long_ex,
            long_bbo,
        })
    }

    /// 检查平仓条件 (静态方法)
    fn check_close_condition(state: &SymbolState, config: &FundingArbConfig) -> bool {
        if !state.has_positions() {
            return false;
        }

        let spread = match state.funding_spread() {
            Some(s) => s,
            None => return false,
        };

        // 费率差收窄到阈值以下，平仓
        spread.abs() < config.close_spread
    }

    /// 单币种最大持仓占比 (20%)
    const MAX_POSITION_RATIO: f64 = 0.2;

    /// 计算下单数量: min(max_notional/price, 对手挂单数量/2, max_quantity, 余额限制)
    fn calculate_quantity(
        config: &FundingArbConfig,
        price: Price,
        counter_qty: Quantity,
        max_position_value: f64,
    ) -> Quantity {
        if price <= 0.0 {
            return 0.0;
        }
        let qty_by_notional = config.max_notional / price;
        let qty_by_book = counter_qty / 2.0;
        // 仓位价值限制: 持仓数量 * 价格 <= max_position_value
        let qty_by_balance = max_position_value / price;
        qty_by_notional
            .min(qty_by_book)
            .min(config.max_quantity)
            .min(qty_by_balance)
    }

    /// 生成开仓信号
    /// 做空方: 挂 bid_price 卖单 (taker 成交)
    /// 做多方: 挂 ask_price 买单 (taker 成交)
    fn make_open_signals(
        symbol: &Symbol,
        short_ex: Exchange,
        short_bbo: &BBO,
        long_ex: Exchange,
        long_bbo: &BBO,
        config: &FundingArbConfig,
        total_balance: f64,
    ) -> Vec<Signal> {
        // 单币种最大持仓价值 = 总余额 * 20%
        let max_position_value = total_balance * Self::MAX_POSITION_RATIO;

        if max_position_value <= 0.0 {
            tracing::warn!(
                symbol = %symbol,
                total_balance = total_balance,
                "Insufficient balance for opening position"
            );
            return vec![];
        }

        // 做空方: 以对手 bid 价格卖出
        let short_price = short_bbo.bid_price;
        let short_qty = Self::calculate_quantity(config, short_price, short_bbo.bid_qty, max_position_value);

        // 做多方: 以对手 ask 价格买入
        let long_price = long_bbo.ask_price;
        let long_qty = Self::calculate_quantity(config, long_price, long_bbo.ask_qty, max_position_value);

        // 取两边最小数量，保证对冲
        let qty = short_qty.min(long_qty);

        if qty <= 0.0 {
            tracing::warn!(
                symbol = %symbol,
                short_qty = short_qty,
                long_qty = long_qty,
                "Calculated quantity is zero or negative"
            );
            return vec![];
        }

        tracing::info!(
            symbol = %symbol,
            short_ex = %short_ex,
            short_price = short_price,
            long_ex = %long_ex,
            long_price = long_price,
            qty = qty,
            "Opening positions"
        );

        vec![
            Signal::PlaceOrder(Order {
                id: String::new(),
                exchange: short_ex,
                symbol: symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Limit {
                    price: short_price,
                    tif: TimeInForce::IOC,
                },
                quantity: qty,
                reduce_only: false,
                client_order_id: Some(Uuid::new_v4().to_string()),
            }),
            Signal::PlaceOrder(Order {
                id: String::new(),
                exchange: long_ex,
                symbol: symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price: long_price,
                    tif: TimeInForce::IOC,
                },
                quantity: qty,
                reduce_only: false,
                client_order_id: Some(Uuid::new_v4().to_string()),
            }),
        ]
    }

    /// 生成平仓信号 (使用限价单保护)
    fn make_close_signals(state: &SymbolState) -> Vec<Signal> {
        let mut signals = Vec::new();

        for (exchange, pos) in &state.positions {
            if !pos.is_empty() {
                // 获取 BBO 用于限价单价格
                let order_type = if let Some(bbo) = state.bbo(*exchange) {
                    // 平多头 (卖出) 用 bid_price，平空头 (买入) 用 ask_price
                    let price = match pos.side {
                        Side::Long => bbo.bid_price,
                        Side::Short => bbo.ask_price,
                    };
                    OrderType::Limit {
                        price,
                        tif: TimeInForce::IOC,
                    }
                } else {
                    // 无 BBO 时回退到市价单
                    OrderType::Market
                };

                signals.push(Signal::PlaceOrder(Order {
                    id: String::new(),
                    exchange: *exchange,
                    symbol: state.symbol.clone(),
                    side: pos.side.opposite(),
                    order_type,
                    quantity: pos.size,
                    reduce_only: true,
                    client_order_id: None,
                }));
            }
        }

        signals
    }

    /// 生成敞口修复信号 (平掉不平衡的持仓)
    fn make_hedge_repair_signal(state: &SymbolState) -> Vec<Signal> {
        if let Some((exchange, side, qty)) = state.unhedged_exposure() {
            tracing::warn!(
                symbol = %state.symbol,
                exchange = %exchange,
                side = %side,
                qty = qty,
                "Detected unhedged exposure, generating repair signal"
            );

            // 获取 BBO 用于限价单
            let order_type = if let Some(bbo) = state.bbo(exchange) {
                let price = match side {
                    Side::Long => bbo.bid_price,  // 平多用 bid
                    Side::Short => bbo.ask_price, // 平空用 ask
                };
                OrderType::Limit {
                    price,
                    tif: TimeInForce::IOC,
                }
            } else {
                OrderType::Market
            };

            return vec![Signal::PlaceOrder(Order {
                id: String::new(),
                exchange,
                symbol: state.symbol.clone(),
                side: side.opposite(),
                order_type,
                quantity: qty,
                reduce_only: true,
                client_order_id: None,
            })];
        }
        vec![]
    }
}

impl Strategy for FundingArbStrategy {
    fn exchanges(&self) -> Vec<Exchange> {
        self.exchanges.clone()
    }

    fn symbols(&self) -> Vec<Symbol> {
        vec![self.symbol.clone()]
    }

    fn market_data_types(&self) -> Vec<MarketDataType> {
        vec![
            MarketDataType::FundingRate,
            MarketDataType::BBO,
            MarketDataType::Position,
            MarketDataType::OrderUpdate,
            MarketDataType::Balance,
        ]
    }

    fn on_event(&mut self, event: ExchangeEvent) -> Vec<Signal> {
        // 处理 BalanceUpdate (全局状态，不走 SymbolState)
        if let ExchangeEvent::BalanceUpdate { exchange, balance, .. } = &event {
            if balance.asset == "USDT" {
                tracing::info!(
                    exchange = %exchange,
                    available = balance.available,
                    frozen = balance.frozen,
                    "Received USDT balance update"
                );
                self.usdt_balances.insert(*exchange, balance.available);
            }
            return vec![];
        }

        // 更新 per-symbol 状态
        self.state.apply(event);

        // 有未完成订单或开仓进行中时等待
        if self.state.has_pending_orders() {
            return vec![];
        }

        // 优先级 1: 检测并修复持仓不平衡
        if let Some(false) = self.state.is_hedged() {
            return Self::make_hedge_repair_signal(&self.state);
        }

        // 优先级 2: 检查平仓条件
        if Self::check_close_condition(&self.state, &self.config) {
            return Self::make_close_signals(&self.state);
        }

        // 优先级 3: 检查开仓条件
        if let Some(cond) = Self::check_open_condition(&self.state, &self.config) {
            let signals = Self::make_open_signals(
                &self.symbol,
                cond.short_ex,
                &cond.short_bbo,
                cond.long_ex,
                &cond.long_bbo,
                &self.config,
                self.total_usdt_balance(),
            );
            // 将订单的 client_order_id 加入 pending_orders
            for signal in &signals {
                let Signal::PlaceOrder(order) = signal;
                if let Some(ref client_id) = order.client_order_id {
                    self.state.add_pending_order(client_id.clone());
                }
            }
            return signals;
        }

        vec![]
    }
}

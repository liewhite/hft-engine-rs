use crate::domain::{
    Exchange, Order, OrderType, Price, Quantity, Rate, Side, Symbol, TimeInForce, BBO,
};
use crate::exchange::SubscriptionKind;
use crate::messaging::{IncomeEvent, StateManager, SymbolState};
use crate::strategy::Strategy;
use std::collections::{HashMap, HashSet};

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
    /// 订单超时时间 (毫秒)
    pub order_timeout_ms: u64,
    /// 不平衡修复阈值 - 敞口价值 (USD, 超过该价值视为不平衡)
    pub unhedge_value_threshold: f64,
}

impl Default for FundingArbConfig {
    fn default() -> Self {
        Self {
            min_spread: 0.0005,   // 0.05%
            max_spread: 0.002,    // 0.2%
            close_spread: 0.0002, // 0.02%
            max_notional: 1000.0, // 1000 USDT
            max_quantity: 1.0,
            order_timeout_ms: 10_000, // 10 seconds
            unhedge_value_threshold: 50.0, // 50 USD
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
}

impl FundingArbStrategy {
    pub fn new(config: FundingArbConfig, exchanges: Vec<Exchange>, symbol: Symbol) -> Self {
        Self {
            config,
            exchanges,
            symbol,
        }
    }

    /// 检查开仓条件
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

    /// 检查平仓条件
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

    /// 单币种最大持仓占比 (100%)
    const MAX_POSITION_RATIO: f64 = 1.0;

    /// 计算下单数量
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
        let qty_by_balance = max_position_value / price;
        qty_by_notional
            .min(qty_by_book)
            .min(config.max_quantity)
            .min(qty_by_balance)
    }

    /// 生成开仓订单
    ///
    /// 根据两边交易所的净值计算开仓大小，取较小的那个
    fn make_open_orders(
        symbol: &Symbol,
        short_ex: Exchange,
        short_bbo: &BBO,
        long_ex: Exchange,
        long_bbo: &BBO,
        config: &FundingArbConfig,
        short_equity: f64,
        long_equity: f64,
    ) -> Vec<Order> {
        // 取两边净值较小的那个来计算最大仓位
        let min_equity = short_equity.min(long_equity);
        let max_position_value = min_equity * Self::MAX_POSITION_RATIO;

        if max_position_value <= 0.0 {
            tracing::warn!(
                symbol = %symbol,
                short_equity = short_equity,
                long_equity = long_equity,
                "Insufficient equity for opening position"
            );
            return vec![];
        }

        let short_price = short_bbo.bid_price;
        let short_qty =
            Self::calculate_quantity(config, short_price, short_bbo.bid_qty, max_position_value);

        let long_price = long_bbo.ask_price;
        let long_qty =
            Self::calculate_quantity(config, long_price, long_bbo.ask_qty, max_position_value);

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
            Order {
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
                client_order_id: None,
            },
            Order {
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
                client_order_id: None,
            },
        ]
    }

    /// 生成平仓订单
    fn make_close_orders(state: &SymbolState) -> Vec<Order> {
        let mut orders = Vec::new();

        for (exchange, pos) in &state.positions {
            if !pos.is_empty() {
                let pos_side = pos.side();
                let order_type = if let Some(bbo) = state.bbo(*exchange) {
                    let price = match pos_side {
                        Side::Long => bbo.bid_price,
                        Side::Short => bbo.ask_price,
                    };
                    OrderType::Limit {
                        price,
                        tif: TimeInForce::IOC,
                    }
                } else {
                    OrderType::Market
                };

                orders.push(Order {
                    id: String::new(),
                    exchange: *exchange,
                    symbol: state.symbol.clone(),
                    side: pos_side.opposite(),
                    order_type,
                    quantity: pos.size.abs(),
                    reduce_only: true,
                    client_order_id: None,
                });
            }
        }

        orders
    }

    /// 检查净敞口是否超过阈值
    ///
    /// 参数:
    /// - net_exposure: 净持仓量 (正数净多头，负数净空头)
    /// - price: 估算价格
    /// - value_threshold: 敞口价值阈值 (USD)
    ///
    /// 返回: 敞口价值是否超过阈值
    fn is_significantly_unhedged(net_exposure: f64, price: f64, value_threshold: f64) -> bool {
        if price <= 0.0 {
            return false;
        }
        let exposure_value = net_exposure.abs() * price;
        exposure_value > value_threshold
    }

    /// 生成敞口修复订单
    ///
    /// 根据净敞口选择资费最优的交易所下单:
    /// - 净多头 (net > 0): 在资费最高的交易所开空单对冲
    /// - 净空头 (net < 0): 在资费最低的交易所开多单对冲
    fn make_hedge_repair_orders(net_exposure: f64, state: &SymbolState) -> Vec<Order> {
        let qty = net_exposure.abs();
        if qty < 1e-10 {
            return vec![];
        }

        // 净多头 → 开空单对冲 → 选资费最高的交易所
        // 净空头 → 开多单对冲 → 选资费最低的交易所
        let (exchange, side) = if net_exposure > 0.0 {
            match state.best_short_exchange() {
                Some((ex, _)) => (ex, Side::Short),
                None => return vec![],
            }
        } else {
            match state.best_long_exchange() {
                Some((ex, _)) => (ex, Side::Long),
                None => return vec![],
            }
        };

        let order_type = if let Some(bbo) = state.bbo(exchange) {
            let price = match side {
                Side::Long => bbo.ask_price,
                Side::Short => bbo.bid_price,
            };
            OrderType::Limit {
                price,
                tif: TimeInForce::IOC,
            }
        } else {
            OrderType::Market
        };

        tracing::warn!(
            symbol = %state.symbol,
            exchange = %exchange,
            side = %side,
            qty = qty,
            net_exposure = net_exposure,
            "Detected unhedged exposure, generating repair order"
        );

        vec![Order {
            id: String::new(),
            exchange,
            symbol: state.symbol.clone(),
            side,
            order_type,
            quantity: qty,
            reduce_only: false,
            client_order_id: None,
        }]
    }
}

impl Strategy for FundingArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let kinds: HashSet<SubscriptionKind> = [
            SubscriptionKind::FundingRate { symbol: self.symbol.clone() },
            SubscriptionKind::BBO { symbol: self.symbol.clone() },
        ]
        .into_iter()
        .collect();

        let mut streams = HashMap::new();
        for exchange in &self.exchanges {
            streams.insert(*exchange, kinds.clone());
        }

        streams
    }

    fn order_timeout_ms(&self) -> u64 {
        self.config.order_timeout_ms
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &mut StateManager) {
        tracing::info!(
            symbol = %self.symbol,
            event  = ?event,
            "Processing event for FundingArbStrategy"
        );
        // 获取本策略关注的 symbol 状态
        let symbol_state = match state.symbol_state(&self.symbol) {
            Some(s) => s,
            None => return,
        };

        // 有未完成订单时等待
        if symbol_state.has_pending_orders() {
            return;
        }

        // 计算净敞口
        let (net_exposure, price) = symbol_state.net_exposure();

        // 优先级 1: 检测并修复显著的持仓不平衡
        if Self::is_significantly_unhedged(
            net_exposure,
            price,
            self.config.unhedge_value_threshold,
        ) {
            let orders = Self::make_hedge_repair_orders(net_exposure, symbol_state);
            state.place_orders(orders);
            return;
        }

        // 优先级 2: 检查平仓条件
        if Self::check_close_condition(symbol_state, &self.config) {
            let orders = Self::make_close_orders(symbol_state);
            state.place_orders(orders);
            return;
        }

        // 优先级 3: 检查开仓条件
        if let Some(cond) = Self::check_open_condition(symbol_state, &self.config) {
            let orders = Self::make_open_orders(
                &self.symbol,
                cond.short_ex,
                &cond.short_bbo,
                cond.long_ex,
                &cond.long_bbo,
                &self.config,
                state.equity(cond.short_ex),
                state.equity(cond.long_ex),
            );
            state.place_orders(orders);
        }
    }
}

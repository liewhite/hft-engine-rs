use crate::domain::{Exchange, Order, OrderId, OrderType, Quantity, Rate, Side, Symbol};
use crate::messaging::{ExchangeEvent, SymbolState};
use crate::strategy::{MarketDataType, Signal, Strategy};
use rust_decimal::Decimal;
use std::collections::HashMap;

/// 资金费率套利策略配置
#[derive(Debug, Clone)]
pub struct FundingArbConfig {
    /// 最小费率差 (开仓阈值)
    pub min_spread: Rate,
    /// 最大费率差 (限制风险)
    pub max_spread: Rate,
    /// 平仓费率差阈值
    pub close_spread: Rate,
    /// 基础下单数量
    pub base_quantity: Quantity,
    /// 最大持仓数量
    pub max_quantity: Quantity,
}

impl Default for FundingArbConfig {
    fn default() -> Self {
        Self {
            min_spread: Rate(Decimal::new(5, 4)),        // 0.0005 = 0.05%
            max_spread: Rate(Decimal::new(20, 4)),       // 0.002 = 0.2%
            close_spread: Rate(Decimal::new(2, 4)),      // 0.0002 = 0.02%
            base_quantity: Quantity(Decimal::new(1, 2)), // 0.01
            max_quantity: Quantity(Decimal::new(1, 0)),  // 1.0
        }
    }
}

/// 资金费率套利策略
pub struct FundingArbStrategy {
    config: FundingArbConfig,
    exchanges: Vec<Exchange>,
    symbols: Vec<Symbol>,
    /// Per-symbol 状态
    states: HashMap<Symbol, SymbolState>,
}

impl FundingArbStrategy {
    pub fn new(config: FundingArbConfig, exchanges: Vec<Exchange>, symbols: Vec<Symbol>) -> Self {
        let mut states = HashMap::new();
        for symbol in &symbols {
            states.insert(symbol.clone(), SymbolState::new(symbol.clone()));
        }

        Self {
            config,
            exchanges,
            symbols,
            states,
        }
    }

    /// 检查开仓条件 (静态方法，避免借用冲突)
    fn check_open_condition(
        state: &SymbolState,
        config: &FundingArbConfig,
    ) -> Option<(Exchange, Exchange)> {
        let spread = state.funding_spread()?;

        if spread.0.abs() < config.min_spread.0 {
            return None;
        }

        if spread.0.abs() > config.max_spread.0 {
            tracing::warn!(
                symbol = %state.symbol,
                spread = %spread,
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

        tracing::info!(
            symbol = %state.symbol,
            spread = %spread,
            short_exchange = %short_ex,
            short_rate = %short_rate.rate,
            long_exchange = %long_ex,
            long_rate = %long_rate.rate,
            "Opening condition met"
        );

        Some((short_ex, long_ex))
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
        spread.0.abs() < config.close_spread.0
    }

    /// 生成开仓信号 (静态方法)
    fn make_open_signals(
        symbol: &Symbol,
        short_ex: Exchange,
        long_ex: Exchange,
        qty: Quantity,
    ) -> Vec<Signal> {
        vec![
            Signal::PlaceOrder(Order {
                id: OrderId::from(""),
                exchange: short_ex,
                symbol: symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Market,
                quantity: qty,
                reduce_only: false,
                client_order_id: None,
            }),
            Signal::PlaceOrder(Order {
                id: OrderId::from(""),
                exchange: long_ex,
                symbol: symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Market,
                quantity: qty,
                reduce_only: false,
                client_order_id: None,
            }),
        ]
    }

    /// 生成平仓信号 (静态方法)
    fn make_close_signals(state: &SymbolState) -> Vec<Signal> {
        let mut signals = Vec::new();

        for (exchange, pos) in &state.positions {
            if !pos.is_empty() {
                signals.push(Signal::PlaceOrder(Order {
                    id: OrderId::from(""),
                    exchange: *exchange,
                    symbol: state.symbol.clone(),
                    side: pos.side.opposite(),
                    order_type: OrderType::Market,
                    quantity: pos.size,
                    reduce_only: true,
                    client_order_id: None,
                }));
            }
        }

        signals
    }
}

impl Strategy for FundingArbStrategy {
    fn exchanges(&self) -> Vec<Exchange> {
        self.exchanges.clone()
    }

    fn symbols(&self) -> Vec<Symbol> {
        self.symbols.clone()
    }

    fn market_data_types(&self) -> Vec<MarketDataType> {
        vec![
            MarketDataType::FundingRate,
            MarketDataType::BBO,
            MarketDataType::Position,
            MarketDataType::OrderUpdate,
        ]
    }

    fn on_event(&mut self, event: ExchangeEvent) -> Vec<Signal> {
        // 获取事件关联的 symbol
        let symbol = match event.symbol() {
            Some(s) => s.clone(),
            None => return vec![], // Balance 事件暂不处理
        };

        // 更新状态
        if let Some(state) = self.states.get_mut(&symbol) {
            state.apply(event);
        } else {
            return vec![];
        }

        // 重新获取不可变引用进行检查
        let state = self.states.get(&symbol).unwrap();

        // 检查开仓条件
        if let Some((short_ex, long_ex)) = Self::check_open_condition(state, &self.config) {
            return Self::make_open_signals(&symbol, short_ex, long_ex, self.config.base_quantity);
        }

        // 检查平仓条件
        if Self::check_close_condition(state, &self.config) {
            return Self::make_close_signals(state);
        }

        vec![]
    }
}

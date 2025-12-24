use crate::domain::{Exchange, ExchangeError, OrderType, Quantity, Rate, Side};
use crate::exchange::ExchangeAdapter;
use crate::messaging::SymbolState;
use crate::strategy::api::{Signal, Strategy, TradeAction};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::sync::Arc;

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
            min_spread: Rate(Decimal::new(5, 4)),      // 0.0005 = 0.05%
            max_spread: Rate(Decimal::new(20, 4)),     // 0.002 = 0.2%
            close_spread: Rate(Decimal::new(2, 4)),    // 0.0002 = 0.02%
            base_quantity: Quantity(Decimal::new(1, 2)), // 0.01
            max_quantity: Quantity(Decimal::new(1, 0)),  // 1.0
        }
    }
}

/// 资金费率套利策略
pub struct FundingArbStrategy<B, O>
where
    B: ExchangeAdapter,
    O: ExchangeAdapter,
{
    config: FundingArbConfig,
    binance: Arc<B>,
    okx: Arc<O>,
}

impl<B, O> FundingArbStrategy<B, O>
where
    B: ExchangeAdapter,
    O: ExchangeAdapter,
{
    pub fn new(config: FundingArbConfig, binance: Arc<B>, okx: Arc<O>) -> Self {
        Self {
            config,
            binance,
            okx,
        }
    }

    /// 判断是否满足开仓条件
    fn should_open(&self, state: &SymbolState) -> Option<(Exchange, Exchange)> {
        let spread = state.funding_spread()?;

        if spread.0.abs() < self.config.min_spread.0 {
            return None;
        }

        if spread.0.abs() > self.config.max_spread.0 {
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

    /// 判断是否满足平仓条件
    fn should_close(&self, state: &SymbolState) -> bool {
        if !state.has_positions() {
            return false;
        }

        let spread = match state.funding_spread() {
            Some(s) => s,
            None => return false,
        };

        // 费率差收窄到阈值以下，平仓
        spread.0.abs() < self.config.close_spread.0
    }
}

impl<B, O> Clone for FundingArbStrategy<B, O>
where
    B: ExchangeAdapter,
    O: ExchangeAdapter,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            binance: self.binance.clone(),
            okx: self.okx.clone(),
        }
    }
}

#[async_trait]
impl<B, O> Strategy for FundingArbStrategy<B, O>
where
    B: ExchangeAdapter + 'static,
    O: ExchangeAdapter + 'static,
{
    fn evaluate(&self, state: &SymbolState) -> Option<Signal> {
        // 检查开仓条件
        if let Some((short_ex, long_ex)) = self.should_open(state) {
            let qty = self.config.base_quantity;
            return Some(Signal {
                symbol: state.symbol.clone(),
                actions: vec![
                    TradeAction::open(short_ex, Side::Short, qty, OrderType::Market),
                    TradeAction::open(long_ex, Side::Long, qty, OrderType::Market),
                ],
            });
        }

        // 检查平仓条件
        if self.should_close(state) {
            let mut actions = Vec::new();

            for (exchange, pos) in &state.positions {
                if !pos.is_empty() {
                    actions.push(TradeAction::close(
                        *exchange,
                        pos.side,
                        pos.size,
                        OrderType::Market,
                    ));
                }
            }

            if !actions.is_empty() {
                return Some(Signal {
                    symbol: state.symbol.clone(),
                    actions,
                });
            }
        }

        None
    }

    async fn execute(&self, signal: Signal) -> Result<(), ExchangeError> {
        use crate::domain::{Order, OrderId};

        for action in signal.actions {
            let order = Order {
                id: OrderId::from(""),
                exchange: action.exchange,
                symbol: signal.symbol.clone(),
                side: action.side,
                order_type: action.order_type,
                quantity: action.quantity,
                reduce_only: action.reduce_only,
                client_order_id: None,
            };

            let result = match action.exchange {
                Exchange::Binance => self.binance.place_order(order).await,
                Exchange::OKX => self.okx.place_order(order).await,
            };

            match result {
                Ok(order_id) => {
                    tracing::info!(
                        symbol = %signal.symbol,
                        exchange = %action.exchange,
                        order_id = %order_id,
                        side = %action.side,
                        quantity = %action.quantity,
                        "Order placed successfully"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        symbol = %signal.symbol,
                        exchange = %action.exchange,
                        error = %e,
                        "Failed to place order"
                    );
                    return Err(e);
                }
            }
        }

        Ok(())
    }
}

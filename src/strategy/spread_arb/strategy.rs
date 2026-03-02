use crate::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};

use super::config::SpreadArbConfig;

/// IBKR 股票 vs Hyperliquid 永续合约的价差套利策略 (单 symbol)
///
/// 策略逻辑：
/// - spread = (HL_bid - IBKR_ask) / IBKR_ask
/// - spread > open_threshold → 开仓 (IBKR 买入 + HL 做空)
/// - spread < close_threshold → 平仓
/// - 平仓优先于开仓
pub struct SpreadArbStrategy {
    config: SpreadArbConfig,
    symbol: Symbol,
}

impl SpreadArbStrategy {
    pub fn new(config: SpreadArbConfig, symbol: Symbol) -> Self {
        Self { config, symbol }
    }

    /// 计算 spread = (HL_bid - IBKR_ask) / IBKR_ask
    fn calc_spread(&self, hl_bid: f64, ibkr_ask: f64) -> Option<f64> {
        if ibkr_ask <= 0.0 {
            return None;
        }
        Some((hl_bid - ibkr_ask) / ibkr_ask)
    }

    /// 计算当前杠杆率 = 仓位价值 / equity
    fn current_leverage(&self, position_value: f64, state: &StateManager) -> f64 {
        let hl_equity = state.equity(Exchange::Hyperliquid);
        if hl_equity <= 0.0 {
            return f64::MAX;
        }
        position_value / hl_equity
    }
}

impl Strategy for SpreadArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let bbo = SubscriptionKind::BBO {
            symbol: self.symbol.clone(),
        };

        let mut streams = HashMap::new();
        streams.insert(Exchange::IBKR, [bbo.clone()].into_iter().collect());
        streams.insert(Exchange::Hyperliquid, [bbo].into_iter().collect());
        streams
    }

    fn order_timeout_ms(&self) -> u64 {
        self.config.order_timeout_ms
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent> {
        // 只响应 BBO 事件
        if !matches!(&event.data, ExchangeEventData::BBO(_)) {
            return vec![];
        }

        let symbol_state = match state.symbol_state(&self.symbol) {
            Some(s) => s,
            None => return vec![],
        };

        // 需要两边 BBO 都就绪
        let ibkr_bbo = match symbol_state.bbo(Exchange::IBKR) {
            Some(b) if b.ask_price > 0.0 && b.bid_price > 0.0 => b,
            _ => return vec![],
        };
        let hl_bbo = match symbol_state.bbo(Exchange::Hyperliquid) {
            Some(b) if b.ask_price > 0.0 && b.bid_price > 0.0 => b,
            _ => return vec![],
        };

        // 跳过有 pending orders 的情况
        if symbol_state.has_pending_orders() {
            return vec![];
        }

        // 计算 spread
        let spread = match self.calc_spread(hl_bbo.bid_price, ibkr_bbo.ask_price) {
            Some(s) => s,
            None => return vec![],
        };

        // 获取当前持仓
        let ibkr_pos = symbol_state
            .position(Exchange::IBKR)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let hl_pos = symbol_state
            .position(Exchange::Hyperliquid)
            .map(|p| p.size)
            .unwrap_or(0.0);

        let has_position = ibkr_pos > 1e-10 || hl_pos < -1e-10;

        // === 平仓优先 ===
        if has_position && spread < self.config.close_threshold {
            let close_qty = ibkr_pos.min(hl_pos.abs());
            if close_qty < 1e-10 {
                return vec![];
            }

            let ibkr_price = ibkr_bbo.bid_price * (1.0 - self.config.ioc_slippage);
            let hl_price = hl_bbo.ask_price * (1.0 + self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.symbol,
                spread = format!("{:.4}%", spread * 100.0),
                close_threshold = format!("{:.4}%", self.config.close_threshold * 100.0),
                close_qty,
                ibkr_bid = ibkr_bbo.bid_price,
                hl_ask = hl_bbo.ask_price,
                "SpreadArb: closing position"
            );

            return vec![
                // IBKR: 卖出平仓
                OutcomeEvent::PlaceOrder {
                    order: Order {
                        id: String::new(),
                        exchange: Exchange::IBKR,
                        symbol: self.symbol.clone(),
                        side: Side::Short,
                        order_type: OrderType::Limit {
                            price: ibkr_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: close_qty,
                        reduce_only: true,
                        client_order_id: String::new(),
                    },
                    comment: format!(
                        "spread_close | spread={:.4}% | qty={:.4} | ibkr_bid={:.4}",
                        spread * 100.0, close_qty, ibkr_bbo.bid_price,
                    ),
                },
                // HL: 买入平空
                OutcomeEvent::PlaceOrder {
                    order: Order {
                        id: String::new(),
                        exchange: Exchange::Hyperliquid,
                        symbol: self.symbol.clone(),
                        side: Side::Long,
                        order_type: OrderType::Limit {
                            price: hl_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: close_qty,
                        reduce_only: true,
                        client_order_id: String::new(),
                    },
                    comment: format!(
                        "spread_close | spread={:.4}% | qty={:.4} | hl_ask={:.4}",
                        spread * 100.0, close_qty, hl_bbo.ask_price,
                    ),
                },
            ];
        }

        // === 开仓 ===
        if spread > self.config.open_threshold {
            // 杠杆率检查：当前仓位价值 + 新仓位价值 不超过 max_leverage
            let current_pos_value = hl_pos.abs() * hl_bbo.bid_price;
            let new_order_value = self.config.order_usd_value;
            let new_total_value = current_pos_value + new_order_value;
            let leverage = self.current_leverage(new_total_value, state);

            if leverage > self.config.max_leverage {
                tracing::debug!(
                    symbol = %self.symbol,
                    leverage = format!("{:.2}", leverage),
                    max_leverage = format!("{:.2}", self.config.max_leverage),
                    "SpreadArb: leverage limit reached, skip opening"
                );
                return vec![];
            }

            let qty = (self.config.order_usd_value / ibkr_bbo.ask_price).floor();
            if qty < 1.0 {
                return vec![];
            }

            let ibkr_price = ibkr_bbo.ask_price * (1.0 + self.config.ioc_slippage);
            let hl_price = hl_bbo.bid_price * (1.0 - self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.symbol,
                spread = format!("{:.4}%", spread * 100.0),
                open_threshold = format!("{:.4}%", self.config.open_threshold * 100.0),
                qty,
                ibkr_ask = ibkr_bbo.ask_price,
                hl_bid = hl_bbo.bid_price,
                leverage = format!("{:.2}", leverage),
                "SpreadArb: opening position"
            );

            return vec![
                // IBKR: 买入
                OutcomeEvent::PlaceOrder {
                    order: Order {
                        id: String::new(),
                        exchange: Exchange::IBKR,
                        symbol: self.symbol.clone(),
                        side: Side::Long,
                        order_type: OrderType::Limit {
                            price: ibkr_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: qty,
                        reduce_only: false,
                        client_order_id: String::new(),
                    },
                    comment: format!(
                        "spread_open | spread={:.4}% | qty={:.4} | ibkr_ask={:.4}",
                        spread * 100.0, qty, ibkr_bbo.ask_price,
                    ),
                },
                // HL: 做空
                OutcomeEvent::PlaceOrder {
                    order: Order {
                        id: String::new(),
                        exchange: Exchange::Hyperliquid,
                        symbol: self.symbol.clone(),
                        side: Side::Short,
                        order_type: OrderType::Limit {
                            price: hl_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: qty,
                        reduce_only: false,
                        client_order_id: String::new(),
                    },
                    comment: format!(
                        "spread_open | spread={:.4}% | qty={:.4} | hl_bid={:.4}",
                        spread * 100.0, qty, hl_bbo.bid_price,
                    ),
                },
            ];
        }

        vec![]
    }
}

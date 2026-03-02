use crate::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};

use super::config::SpreadArbConfig;

/// 仓位比较的 epsilon（用于判断仓位是否为零）
const POSITION_EPSILON: f64 = 1e-10;

/// IBKR 股票 vs Hyperliquid 永续合约的价差套利策略 (单 symbol)
///
/// 策略逻辑：
/// - spread = (HL_bid - IBKR_ask) / IBKR_ask
/// - spread > open_threshold → 开仓 (IBKR 买入 + HL 做空)
/// - spread < close_threshold → 平仓
/// - 敞口 rebalance 优先 > 平仓 > 开仓
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

    /// 计算当前杠杆率 = 仓位价值 / HL equity
    ///
    /// 只检查 HL 侧杠杆率，因为 IBKR 是现金账户（股票买入），
    /// 其"杠杆"受限于账户现金余额，由 IBKR 本身的保证金系统控制。
    /// HL 侧是永续合约，杠杆率是策略需要主动管理的风险指标。
    fn current_leverage(&self, position_value: f64, state: &StateManager) -> f64 {
        let hl_equity = state.equity(Exchange::Hyperliquid);
        if hl_equity <= 0.0 {
            return f64::MAX;
        }
        position_value / hl_equity
    }

    /// 检查两腿敞口是否需要 rebalance
    ///
    /// 两腿 IOC 订单可能部分成交导致单边敞口。
    /// 检测 IBKR 多头与 HL 空头数量差异，平掉多余的一腿。
    fn check_rebalance(
        &self,
        ibkr_pos: f64,
        hl_pos: f64,
        ibkr_bid: f64,
        hl_ask: f64,
    ) -> Vec<OutcomeEvent> {
        // ibkr_pos >= 0 (多头), hl_pos <= 0 (空头)
        // 理想状态: ibkr_pos == hl_pos.abs()
        let exposure = ibkr_pos - hl_pos.abs();

        if exposure.abs() < 1.0 {
            // 股票最小单位 1 股，差异不足 1 股无需 rebalance
            return vec![];
        }

        if exposure > 0.0 {
            // IBKR 多头多了，需要卖出 IBKR 多余的部分
            let rebal_qty = exposure.floor();
            if rebal_qty < 1.0 {
                return vec![];
            }

            let price = ibkr_bid * (1.0 - self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.symbol,
                ibkr_pos,
                hl_pos,
                exposure,
                rebal_qty,
                "SpreadArb: rebalance — IBKR long excess, selling"
            );

            vec![OutcomeEvent::PlaceOrder {
                order: Order {
                    id: String::new(),
                    exchange: Exchange::IBKR,
                    symbol: self.symbol.clone(),
                    side: Side::Short,
                    order_type: OrderType::Limit {
                        price,
                        tif: TimeInForce::IOC,
                    },
                    quantity: rebal_qty,
                    reduce_only: true,
                    client_order_id: String::new(),
                },
                comment: format!(
                    "spread_rebal | exp={:.4} | qty={:.4} | ibkr_bid={:.4}",
                    exposure, rebal_qty, ibkr_bid,
                ),
            }]
        } else {
            // HL 空头多了，需要买入 HL 平掉多余的部分
            let rebal_qty = exposure.abs().floor();
            if rebal_qty < POSITION_EPSILON {
                return vec![];
            }

            let price = hl_ask * (1.0 + self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.symbol,
                ibkr_pos,
                hl_pos,
                exposure,
                rebal_qty,
                "SpreadArb: rebalance — HL short excess, buying"
            );

            vec![OutcomeEvent::PlaceOrder {
                order: Order {
                    id: String::new(),
                    exchange: Exchange::Hyperliquid,
                    symbol: self.symbol.clone(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price,
                        tif: TimeInForce::IOC,
                    },
                    quantity: rebal_qty,
                    reduce_only: true,
                    client_order_id: String::new(),
                },
                comment: format!(
                    "spread_rebal | exp={:.4} | qty={:.4} | hl_ask={:.4}",
                    exposure, rebal_qty, hl_ask,
                ),
            }]
        }
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

        // 获取当前持仓
        let ibkr_pos = symbol_state
            .position(Exchange::IBKR)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let hl_pos = symbol_state
            .position(Exchange::Hyperliquid)
            .map(|p| p.size)
            .unwrap_or(0.0);

        let has_position = ibkr_pos > POSITION_EPSILON || hl_pos < -POSITION_EPSILON;

        // === 步骤 1: 敞口 rebalance 优先 ===
        if has_position {
            let rebalance_orders = self.check_rebalance(
                ibkr_pos,
                hl_pos,
                ibkr_bbo.bid_price,
                hl_bbo.ask_price,
            );
            if !rebalance_orders.is_empty() {
                return rebalance_orders;
            }
        }

        // 计算 spread
        let spread = match self.calc_spread(hl_bbo.bid_price, ibkr_bbo.ask_price) {
            Some(s) => s,
            None => return vec![],
        };

        // === 步骤 2: 平仓 ===
        if has_position && spread < self.config.close_threshold {
            let close_qty = ibkr_pos.min(hl_pos.abs());
            if close_qty < POSITION_EPSILON {
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

        // === 步骤 3: 开仓 ===
        if spread > self.config.open_threshold {
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

            // IBKR 股票最小单位 1 股，向下取整
            // HL 永续合约两边使用相同数量（股数），具体精度由交易所下单时校验
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

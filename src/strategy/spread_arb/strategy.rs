use crate::domain::{now_ms, Exchange, MarketStatus, Order, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};

use super::config::SpreadArbConfig;

/// 仓位比较的 epsilon（用于判断仓位是否为零）
const POSITION_EPSILON: f64 = 1e-10;

/// 最小下单数量（IBKR 股票最小单位 1 股）
const MIN_ORDER_QTY: f64 = 1.0;

/// IBKR 股票 vs Hyperliquid 永续合约的价差套利策略 (单 symbol)
///
/// 策略逻辑：
/// - spread = (HL_bid - IBKR_ask) / IBKR_ask
/// - spread > open_threshold → 开仓 (IBKR 买入 + HL 做空)
/// - spread < close_threshold → 平仓
/// - 敞口 rebalance 优先 > 平仓 > 开仓
pub struct SpreadArbStrategy {
    config: SpreadArbConfig,
    /// IBKR 侧 symbol (e.g., "AAPL")
    ibkr_symbol: Symbol,
    /// Hyperliquid 侧 symbol (e.g., "xyz:AAPL")
    hl_symbol: Symbol,
}

impl SpreadArbStrategy {
    pub fn new(config: SpreadArbConfig, symbol: Symbol, hl_dex: &str) -> Self {
        config.validate();
        let hl_symbol = if hl_dex.is_empty() {
            symbol.clone()
        } else {
            format!("{}:{}", hl_dex, symbol)
        };
        Self {
            config,
            ibkr_symbol: symbol,
            hl_symbol,
        }
    }

    /// 计算开仓 spread = (HL_bid - IBKR_ask) / IBKR_ask
    /// 开仓时: IBKR 按 ask 买入, HL 按 bid 卖出
    fn calc_open_spread(&self, hl_bid: f64, ibkr_ask: f64) -> Option<f64> {
        if ibkr_ask <= 0.0 {
            return None;
        }
        Some((hl_bid - ibkr_ask) / ibkr_ask)
    }

    /// 计算平仓 spread = (HL_ask - IBKR_bid) / IBKR_bid
    /// 平仓时: IBKR 按 bid 卖出, HL 按 ask 买入
    fn calc_close_spread(&self, hl_ask: f64, ibkr_bid: f64) -> Option<f64> {
        if ibkr_bid <= 0.0 {
            return None;
        }
        Some((hl_ask - ibkr_bid) / ibkr_bid)
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

    /// 检测仓位方向异常，紧急平仓
    ///
    /// 正常状态下 IBKR 应为多头(>=0)，HL 应为空头(<=0)。
    /// 如果出现反方向仓位（IBKR 空头或 HL 多头），说明系统异常，
    /// 立即生成 reduce_only 平仓单，阻止后续正常逻辑执行。
    fn emergency_flatten(
        &self,
        ibkr_pos: f64,
        hl_pos: f64,
        ibkr_ask: f64,
        hl_bid: f64,
    ) -> Option<OutcomeEvent> {
        let ibkr_wrong = ibkr_pos < -POSITION_EPSILON;
        let hl_wrong = hl_pos > POSITION_EPSILON;

        if !ibkr_wrong && !hl_wrong {
            return None;
        }

        tracing::error!(
            symbol = %self.ibkr_symbol,
            ibkr_pos,
            hl_pos,
            "SpreadArb: EMERGENCY — unexpected position direction, flattening"
        );

        let mut orders = Vec::new();

        if ibkr_wrong {
            // IBKR 意外空头 → 买入平仓
            let qty = ibkr_pos.abs().floor();
            if qty >= MIN_ORDER_QTY {
                let price = ibkr_ask * (1.0 + self.config.ioc_slippage);
                orders.push(Order {
                    id: String::new(),
                    exchange: Exchange::IBKR,
                    symbol: self.ibkr_symbol.clone(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price,
                        tif: TimeInForce::IOC,
                    },
                    quantity: qty,
                    reduce_only: true,
                    client_order_id: String::new(),
                });
            }
        }

        if hl_wrong {
            // HL 意外多头 → 卖出平仓
            let qty = hl_pos.floor();
            if qty >= MIN_ORDER_QTY {
                let price = hl_bid * (1.0 - self.config.ioc_slippage);
                orders.push(Order {
                    id: String::new(),
                    exchange: Exchange::Hyperliquid,
                    symbol: self.hl_symbol.clone(),
                    side: Side::Short,
                    order_type: OrderType::Limit {
                        price,
                        tif: TimeInForce::IOC,
                    },
                    quantity: qty,
                    reduce_only: true,
                    client_order_id: String::new(),
                });
            }
        }

        if orders.is_empty() {
            tracing::warn!(
                symbol = %self.ibkr_symbol,
                ibkr_pos,
                hl_pos,
                "SpreadArb: abnormal position detected but qty < 1, strategy halted"
            );
            return None;
        }

        Some(OutcomeEvent::PlaceOrders {
            comment: format!(
                "emergency_flatten | ibkr_pos={:.4} | hl_pos={:.4}",
                ibkr_pos, hl_pos,
            ),
            orders,
        })
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
    ) -> Option<OutcomeEvent> {
        // 净敞口 = 两腿仓位之和
        // ibkr_pos >= 0 (多头), hl_pos <= 0 (空头)
        // 理想对冲: ibkr_pos + hl_pos == 0
        let exposure = ibkr_pos + hl_pos;

        if exposure.abs() < MIN_ORDER_QTY {
            return None;
        }

        let (order, rebal_qty) = if exposure > 0.0 {
            // IBKR 多头多了，需要卖出 IBKR 多余的部分
            let rebal_qty = exposure.floor();
            if rebal_qty < MIN_ORDER_QTY {
                return None;
            }

            let price = ibkr_bid * (1.0 - self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.ibkr_symbol,
                ibkr_pos,
                hl_pos,
                exposure,
                rebal_qty,
                "SpreadArb: rebalance — IBKR long excess, selling"
            );

            let order = Order {
                id: String::new(),
                exchange: Exchange::IBKR,
                symbol: self.ibkr_symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Limit {
                    price,
                    tif: TimeInForce::IOC,
                },
                quantity: rebal_qty,
                reduce_only: true,
                client_order_id: String::new(),
            };
            (order, rebal_qty)
        } else {
            // HL 空头多了，需要买入 HL 平掉多余的部分
            // 当 IBKR 侧实质为零时，直接用 raw qty（executor 会按 HL size_step 取整）
            let rebal_qty = if ibkr_pos.abs() < MIN_ORDER_QTY {
                exposure.abs()
            } else {
                exposure.abs().floor()
            };
            if rebal_qty < POSITION_EPSILON {
                return None;
            }

            let price = hl_ask * (1.0 + self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.ibkr_symbol,
                ibkr_pos,
                hl_pos,
                exposure,
                rebal_qty,
                "SpreadArb: rebalance — HL short excess, buying"
            );

            let order = Order {
                id: String::new(),
                exchange: Exchange::Hyperliquid,
                symbol: self.hl_symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price,
                    tif: TimeInForce::IOC,
                },
                quantity: rebal_qty,
                reduce_only: true,
                client_order_id: String::new(),
            };
            (order, rebal_qty)
        };

        Some(OutcomeEvent::PlaceOrders {
            comment: format!(
                "spread_rebal | exp={:.4} | qty={:.4}",
                exposure, rebal_qty,
            ),
            orders: vec![order],
        })
    }
}

impl Strategy for SpreadArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let ibkr_bbo = SubscriptionKind::BBO {
            symbol: self.ibkr_symbol.clone(),
        };
        let hl_bbo = SubscriptionKind::BBO {
            symbol: self.hl_symbol.clone(),
        };

        let mut streams = HashMap::new();
        streams.insert(Exchange::IBKR, [ibkr_bbo].into_iter().collect());
        streams.insert(Exchange::Hyperliquid, [hl_bbo].into_iter().collect());
        streams
    }

    fn order_timeout_ms(&self) -> u64 {
        self.config.order_timeout_ms
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Option<OutcomeEvent> {
        // 只响应 BBO 事件
        if !matches!(&event.data, ExchangeEventData::BBO(_)) {
            return None;
        }

        // 跨交易所 symbol 不同 (IBKR: "AAPL", HL: "xyz:AAPL")，分别查询
        let ibkr_state = state.symbol_state(&self.ibkr_symbol)?;
        let hl_state = state.symbol_state(&self.hl_symbol)?;

        // 两边市场都必须是 Liquid 才允许下单
        if state.market_status(Exchange::IBKR) != MarketStatus::Liquid
            || state.market_status(Exchange::Hyperliquid) != MarketStatus::Liquid
        {
            return None;
        }

        // 需要两边 BBO 都就绪
        let ibkr_bbo = match ibkr_state.bbo(Exchange::IBKR) {
            Some(b) if b.ask_price > 0.0 && b.bid_price > 0.0 => b,
            _ => return None,
        };
        let hl_bbo = match hl_state.bbo(Exchange::Hyperliquid) {
            Some(b) if b.ask_price > 0.0 && b.bid_price > 0.0 => b,
            _ => return None,
        };

        // BBO 新鲜度检查
        let now = now_ms();
        let staleness = self.config.bbo_staleness_ms;
        if now.saturating_sub(ibkr_bbo.timestamp) > staleness {
            tracing::debug!(
                symbol = %self.ibkr_symbol,
                age_ms = now.saturating_sub(ibkr_bbo.timestamp),
                "SpreadArb: IBKR BBO stale, skipping"
            );
            return None;
        }
        if now.saturating_sub(hl_bbo.timestamp) > staleness {
            tracing::debug!(
                symbol = %self.hl_symbol,
                age_ms = now.saturating_sub(hl_bbo.timestamp),
                "SpreadArb: HL BBO stale, skipping"
            );
            return None;
        }

        // 跳过有 pending orders 的情况 (两侧都要检查)
        if ibkr_state.has_pending_orders() || hl_state.has_pending_orders() {
            return None;
        }

        // 获取当前持仓
        let ibkr_pos = ibkr_state
            .position(Exchange::IBKR)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let hl_pos = hl_state
            .position(Exchange::Hyperliquid)
            .map(|p| p.size)
            .unwrap_or(0.0);

        // === 步骤 0: 仓位方向守卫 ===
        if let Some(signal) = self.emergency_flatten(
            ibkr_pos,
            hl_pos,
            ibkr_bbo.ask_price,
            hl_bbo.bid_price,
        ) {
            return Some(signal);
        }

        let has_position = ibkr_pos.abs() > POSITION_EPSILON || hl_pos.abs() > POSITION_EPSILON;

        // === 步骤 1: 敞口 rebalance 优先 ===
        if has_position {
            if let Some(signal) = self.check_rebalance(
                ibkr_pos,
                hl_pos,
                ibkr_bbo.bid_price,
                hl_bbo.ask_price,
            ) {
                return Some(signal);
            }
        }

        // === 步骤 2: 平仓 (用实际执行侧价格: IBKR bid / HL ask) ===
        let close_spread = self.calc_close_spread(hl_bbo.ask_price, ibkr_bbo.bid_price)?;
        if has_position && close_spread < self.config.close_threshold {
            let spread = close_spread;
            let paired_qty = ibkr_pos.min(hl_pos.abs());
            let close_qty = paired_qty.floor();

            if close_qty >= MIN_ORDER_QTY {
                // 双腿配对平仓
                let ibkr_price = ibkr_bbo.bid_price * (1.0 - self.config.ioc_slippage);
                let hl_price = hl_bbo.ask_price * (1.0 + self.config.ioc_slippage);

                tracing::info!(
                    symbol = %self.ibkr_symbol,
                    spread = format!("{:.4}%", spread * 100.0),
                    close_threshold = format!("{:.4}%", self.config.close_threshold * 100.0),
                    close_qty,
                    ibkr_bid = ibkr_bbo.bid_price,
                    hl_ask = hl_bbo.ask_price,
                    "SpreadArb: closing position"
                );

                return Some(OutcomeEvent::PlaceOrders {
                    comment: format!(
                        "spread_close | spread={:.4}% | qty={:.4} | ibkr_bid={:.4} | hl_ask={:.4}",
                        spread * 100.0, close_qty, ibkr_bbo.bid_price, hl_bbo.ask_price,
                    ),
                    orders: vec![
                        // IBKR: 卖出平仓
                        Order {
                            id: String::new(),
                            exchange: Exchange::IBKR,
                            symbol: self.ibkr_symbol.clone(),
                            side: Side::Short,
                            order_type: OrderType::Limit {
                                price: ibkr_price,
                                tif: TimeInForce::IOC,
                            },
                            quantity: close_qty,
                            reduce_only: true,
                            client_order_id: String::new(),
                        },
                        // HL: 买入平空
                        Order {
                            id: String::new(),
                            exchange: Exchange::Hyperliquid,
                            symbol: self.hl_symbol.clone(),
                            side: Side::Long,
                            order_type: OrderType::Limit {
                                price: hl_price,
                                tif: TimeInForce::IOC,
                            },
                            quantity: close_qty,
                            reduce_only: true,
                            client_order_id: String::new(),
                        },
                    ],
                });
            }

            // 残余清理: IBKR 侧已清零但 HL 侧有残余空头，单独平 HL
            if ibkr_pos.abs() < MIN_ORDER_QTY && hl_pos.abs() > POSITION_EPSILON {
                let hl_close_qty = hl_pos.abs();
                let hl_price = hl_bbo.ask_price * (1.0 + self.config.ioc_slippage);

                tracing::info!(
                    symbol = %self.ibkr_symbol,
                    hl_pos,
                    hl_close_qty,
                    "SpreadArb: closing residual HL position (IBKR already flat)"
                );

                return Some(OutcomeEvent::PlaceOrders {
                    comment: format!(
                        "spread_close_residual_hl | hl_pos={:.4} | qty={:.4}",
                        hl_pos, hl_close_qty,
                    ),
                    orders: vec![Order {
                        id: String::new(),
                        exchange: Exchange::Hyperliquid,
                        symbol: self.hl_symbol.clone(),
                        side: Side::Long,
                        order_type: OrderType::Limit {
                            price: hl_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: hl_close_qty,
                        reduce_only: true,
                        client_order_id: String::new(),
                    }],
                });
            }

            return None;
        }

        // === 步骤 3: 开仓 (用实际执行侧价格: IBKR ask / HL bid) ===
        let open_spread = self.calc_open_spread(hl_bbo.bid_price, ibkr_bbo.ask_price)?;
        if open_spread > self.config.open_threshold {
            let spread = open_spread;
            let current_pos_value = hl_pos.abs() * hl_bbo.bid_price;
            let new_order_value = self.config.order_usd_value;
            let new_total_value = current_pos_value + new_order_value;
            let leverage = self.current_leverage(new_total_value, state);

            if leverage > self.config.max_leverage {
                tracing::debug!(
                    symbol = %self.ibkr_symbol,
                    leverage = format!("{:.2}", leverage),
                    max_leverage = format!("{:.2}", self.config.max_leverage),
                    "SpreadArb: leverage limit reached, skip opening"
                );
                return None;
            }

            // IBKR 股票最小单位 1 股，向下取整
            // HL 永续合约两边使用相同数量（股数），具体精度由交易所下单时校验
            let qty = (self.config.order_usd_value / ibkr_bbo.ask_price).floor();
            if qty < MIN_ORDER_QTY {
                return None;
            }

            let ibkr_price = ibkr_bbo.ask_price * (1.0 + self.config.ioc_slippage);
            let hl_price = hl_bbo.bid_price * (1.0 - self.config.ioc_slippage);

            tracing::info!(
                symbol = %self.ibkr_symbol,
                spread = format!("{:.4}%", spread * 100.0),
                open_threshold = format!("{:.4}%", self.config.open_threshold * 100.0),
                qty,
                ibkr_ask = ibkr_bbo.ask_price,
                hl_bid = hl_bbo.bid_price,
                leverage = format!("{:.2}", leverage),
                "SpreadArb: opening position"
            );

            return Some(OutcomeEvent::PlaceOrders {
                comment: format!(
                    "spread_open | spread={:.4}% | qty={:.4} | ibkr_ask={:.4} | hl_bid={:.4} | lev={:.2}",
                    spread * 100.0, qty, ibkr_bbo.ask_price, hl_bbo.bid_price, leverage,
                ),
                orders: vec![
                    // IBKR: 买入
                    Order {
                        id: String::new(),
                        exchange: Exchange::IBKR,
                        symbol: self.ibkr_symbol.clone(),
                        side: Side::Long,
                        order_type: OrderType::Limit {
                            price: ibkr_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: qty,
                        reduce_only: false,
                        client_order_id: String::new(),
                    },
                    // HL: 做空
                    Order {
                        id: String::new(),
                        exchange: Exchange::Hyperliquid,
                        symbol: self.hl_symbol.clone(),
                        side: Side::Short,
                        order_type: OrderType::Limit {
                            price: hl_price,
                            tif: TimeInForce::IOC,
                        },
                        quantity: qty,
                        reduce_only: false,
                        client_order_id: String::new(),
                    },
                ],
            });
        }

        None
    }
}

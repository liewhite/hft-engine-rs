use crate::domain::{CandleInterval, Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};

use super::config::MacdGridConfig;
use super::indicators::{AtrCalculator, MacdCalculator};

const POSITION_EPSILON: f64 = 1e-10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trend {
    StrongBull,
    WeakBull,
    WeakBear,
    StrongBear,
}

struct GridParams {
    buy_spacing: f64,
    sell_spacing: f64,
    /// 允许的最大多头仓位 (USDT), 0 = 不允许开多
    max_long_usd: f64,
    /// 允许的最大空头仓位 (USDT), 0 = 不允许开空
    max_short_usd: f64,
}

pub struct MacdGridStrategy {
    config: MacdGridConfig,
    symbol: Symbol,
    macd_15m: MacdCalculator,
    macd_1h: MacdCalculator,
    macd_4h: MacdCalculator,
    atr: AtrCalculator,
    /// 上一次下单的参考价格，网格从此价格向两侧延伸
    last_order_price: Option<f64>,
}

impl MacdGridStrategy {
    pub fn new(config: MacdGridConfig) -> Self {
        let symbol = config.symbol.clone();
        Self {
            config,
            symbol,
            macd_15m: MacdCalculator::new(),
            macd_1h: MacdCalculator::new(),
            macd_4h: MacdCalculator::new(),
            atr: AtrCalculator::new(14),
            last_order_price: None,
        }
    }

    fn current_trend(&self) -> Option<Trend> {
        let dea_15m = self.macd_15m.dea()?;
        let dea_1h = self.macd_1h.dea()?;
        let dea_4h = self.macd_4h.dea()?;

        let above_count = [dea_15m, dea_1h, dea_4h]
            .iter()
            .filter(|d| **d > 0.0)
            .count();

        Some(match above_count {
            3 => Trend::StrongBull,
            2 => Trend::WeakBull,
            1 => Trend::WeakBear,
            0 => Trend::StrongBear,
            _ => unreachable!(),
        })
    }

    fn grid_params(&self, trend: Trend, atr: f64) -> GridParams {
        let agg = self.config.aggressive_spacing_factor;
        let con = self.config.conservative_spacing_factor;
        let min = self.config.min_spacing;

        let spacing = |factor: f64| (atr * factor).max(min);

        match trend {
            Trend::StrongBull => GridParams {
                buy_spacing: spacing(agg),
                sell_spacing: spacing(con),
                max_long_usd: self.config.max_position_usd,
                max_short_usd: 0.0,
            },
            Trend::WeakBull => GridParams {
                buy_spacing: spacing(con),
                sell_spacing: spacing(con),
                max_long_usd: self.config.weak_position_usd,
                max_short_usd: 0.0,
            },
            Trend::WeakBear => GridParams {
                buy_spacing: spacing(con),
                sell_spacing: spacing(con),
                max_long_usd: 0.0,
                max_short_usd: self.config.weak_position_usd,
            },
            Trend::StrongBear => GridParams {
                buy_spacing: spacing(con),
                sell_spacing: spacing(agg),
                max_long_usd: 0.0,
                max_short_usd: self.config.max_position_usd,
            },
        }
    }

    fn indicators_ready(&self) -> bool {
        self.macd_15m.is_ready()
            && self.macd_1h.is_ready()
            && self.macd_4h.is_ready()
            && self.atr.is_ready()
    }

    fn feed_candle(&mut self, close: f64, high: f64, low: f64, interval: CandleInterval, confirm: bool) {
        if !confirm {
            return;
        }
        match interval {
            CandleInterval::Min15 => {
                self.macd_15m.update(close);
                self.atr.update(high, low, close);
            }
            CandleInterval::Hour1 => {
                self.macd_1h.update(close);
            }
            CandleInterval::Hour4 => {
                self.macd_4h.update(close);
            }
            _ => {}
        }
    }

    fn check_grid(&mut self, state: &StateManager) -> Option<OutcomeEvent> {
        let symbol_state = state.symbol_state(&self.symbol)?;
        let trend = self.current_trend()?;
        let atr = self.atr.value()?;
        let bbo = symbol_state.bbo(Exchange::OKX)?;
        let mid_price = bbo.mid_price();

        if mid_price <= 0.0 {
            return None;
        }

        let params = self.grid_params(trend, atr);
        let pos_size = symbol_state
            .position(Exchange::OKX)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let long_usd = pos_size.max(0.0) * mid_price;
        let short_usd = (-pos_size).max(0.0) * mid_price;

        // 初始化网格参考价
        if self.last_order_price.is_none() {
            self.last_order_price = Some(mid_price);
            return None;
        }
        let last = self.last_order_price.unwrap();

        // === 买入触发 (价格下穿 last - buy_spacing) ===
        if mid_price <= last - params.buy_spacing {
            // 优先: 有空头仓位 → 买入减仓 (reduce_only)
            if pos_size < -POSITION_EPSILON {
                let qty = (self.config.order_usd_value / bbo.ask_price).min(pos_size.abs());
                self.last_order_price = Some(last - params.buy_spacing);
                return self.make_order(
                    Side::Long,
                    bbo.ask_price,
                    qty,
                    true,
                    &format!(
                        "grid_reduce_short | trend={:?} | pos={:.4} | atr={:.4}",
                        trend, pos_size, atr
                    ),
                );
            }
            // 其次: 允许开多且未超限 → 买入开多
            if params.max_long_usd > 0.0 && long_usd < params.max_long_usd {
                let qty = self.config.order_usd_value / bbo.ask_price;
                self.last_order_price = Some(last - params.buy_spacing);
                return self.make_order(
                    Side::Long,
                    bbo.ask_price,
                    qty,
                    false,
                    &format!(
                        "grid_open_long | trend={:?} | pos={:.4} | atr={:.4}",
                        trend, pos_size, atr
                    ),
                );
            }
            // 无法执行: 不移动 last_order_price，等待下次条件满足
        }

        // === 卖出触发 (价格上穿 last + sell_spacing) ===
        if mid_price >= last + params.sell_spacing {
            // 优先: 有多头仓位 → 卖出减仓 (reduce_only)
            if pos_size > POSITION_EPSILON {
                let qty = (self.config.order_usd_value / bbo.bid_price).min(pos_size);
                self.last_order_price = Some(last + params.sell_spacing);
                return self.make_order(
                    Side::Short,
                    bbo.bid_price,
                    qty,
                    true,
                    &format!(
                        "grid_reduce_long | trend={:?} | pos={:.4} | atr={:.4}",
                        trend, pos_size, atr
                    ),
                );
            }
            // 其次: 允许开空且未超限 → 卖出开空
            if params.max_short_usd > 0.0 && short_usd < params.max_short_usd {
                let qty = self.config.order_usd_value / bbo.bid_price;
                self.last_order_price = Some(last + params.sell_spacing);
                return self.make_order(
                    Side::Short,
                    bbo.bid_price,
                    qty,
                    false,
                    &format!(
                        "grid_open_short | trend={:?} | pos={:.4} | atr={:.4}",
                        trend, pos_size, atr
                    ),
                );
            }
            // 无法执行: 不移动 last_order_price
        }

        None
    }

    fn make_order(
        &self,
        side: Side,
        price: f64,
        qty: f64,
        reduce_only: bool,
        comment: &str,
    ) -> Option<OutcomeEvent> {
        if qty <= 0.0 || price <= 0.0 {
            return None;
        }

        let limit_price = match side {
            Side::Long => price * (1.0 + self.config.ioc_slippage),
            Side::Short => price * (1.0 - self.config.ioc_slippage),
        };

        tracing::info!(
            symbol = %self.symbol,
            side = ?side,
            price = format!("{:.4}", price),
            limit_price = format!("{:.4}", limit_price),
            qty = format!("{:.6}", qty),
            reduce_only,
            comment,
            "Placing grid order"
        );

        Some(OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: Exchange::OKX,
                symbol: self.symbol.clone(),
                side,
                order_type: OrderType::Limit {
                    price: limit_price,
                    tif: TimeInForce::IOC,
                },
                quantity: qty,
                reduce_only,
                client_order_id: String::new(),
            }],
            comment: comment.to_string(),
        })
    }

    fn log_status(&self, state: &StateManager) {
        let symbol_state = match state.symbol_state(&self.symbol) {
            Some(s) => s,
            None => return,
        };

        let mid = symbol_state
            .bbo(Exchange::OKX)
            .map(|b| b.mid_price())
            .unwrap_or(0.0);
        let pos_size = symbol_state
            .position(Exchange::OKX)
            .map(|p| p.size)
            .unwrap_or(0.0);

        tracing::info!(
            symbol = %self.symbol,
            trend = ?self.current_trend(),
            dea_15m = self.macd_15m.dea().map(|d| format!("{:.6}", d)).unwrap_or_default(),
            dea_1h = self.macd_1h.dea().map(|d| format!("{:.6}", d)).unwrap_or_default(),
            dea_4h = self.macd_4h.dea().map(|d| format!("{:.6}", d)).unwrap_or_default(),
            atr = self.atr.value().map(|a| format!("{:.4}", a)).unwrap_or_default(),
            mid_price = format!("{:.4}", mid),
            pos = format!("{:.4}", pos_size),
            last_order_price = self.last_order_price.map(|p| format!("{:.4}", p)).unwrap_or_default(),
            ready = self.indicators_ready(),
            "MacdGrid status"
        );
    }
}

impl Strategy for MacdGridStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let kinds: HashSet<SubscriptionKind> = [
            SubscriptionKind::BBO {
                symbol: self.symbol.clone(),
            },
            SubscriptionKind::Candle {
                symbol: self.symbol.clone(),
                interval: CandleInterval::Min15,
            },
            SubscriptionKind::Candle {
                symbol: self.symbol.clone(),
                interval: CandleInterval::Hour1,
            },
            SubscriptionKind::Candle {
                symbol: self.symbol.clone(),
                interval: CandleInterval::Hour4,
            },
        ]
        .into_iter()
        .collect();

        let mut streams = HashMap::new();
        streams.insert(Exchange::OKX, kinds);
        streams
    }

    fn order_timeout_ms(&self) -> u64 {
        self.config.order_timeout_ms
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Option<OutcomeEvent> {
        match &event.data {
            ExchangeEventData::HistoryCandles(candles) => {
                for candle in candles {
                    if candle.symbol == self.symbol {
                        self.feed_candle(
                            candle.close,
                            candle.high,
                            candle.low,
                            candle.interval,
                            candle.confirm,
                        );
                    }
                }
                tracing::info!(
                    symbol = %self.symbol,
                    count = candles.len(),
                    interval = candles.first().map(|c| c.interval.to_string()).unwrap_or_default(),
                    ready = self.indicators_ready(),
                    "Fed history candles"
                );
                return None;
            }
            ExchangeEventData::Candle(candle) => {
                if candle.symbol == self.symbol {
                    self.feed_candle(
                        candle.close,
                        candle.high,
                        candle.low,
                        candle.interval,
                        candle.confirm,
                    );
                }
                return None;
            }
            ExchangeEventData::Clock => {
                self.log_status(state);
                return None;
            }
            ExchangeEventData::BBO(_) => {}
            _ => return None,
        }

        if !self.indicators_ready() {
            return None;
        }

        let symbol_state = state.symbol_state(&self.symbol)?;
        if symbol_state.has_pending_orders() {
            return None;
        }

        self.check_grid(state)
    }
}

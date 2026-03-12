#[allow(unused_imports)]
use crate::domain::{CandleInterval, Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};

use super::config::MacdGridConfig;
use super::indicators::{AtrCalculator, MacdCalculator};

#[allow(unused)]
const POSITION_EPSILON: f64 = 1e-10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trend {
    StrongBull,
    WeakBull,
    WeakBear,
    StrongBear,
}

struct GridParams {
    /// 买入方向 ATR 基础因子 (趋势决定)
    buy_base: f64,
    /// 卖出方向 ATR 基础因子 (趋势决定)
    sell_base: f64,
    /// 允许的最大多头仓位 (USDT), 0 = 不允许开多
    max_long_usd: f64,
    /// 允许的最大空头仓位 (USDT), 0 = 不允许开空
    max_short_usd: f64,
}

pub struct MacdGridStrategy {
    config: MacdGridConfig,
    macd_15m: MacdCalculator,
    macd_1h: MacdCalculator,
    macd_4h: MacdCalculator,
    atr: AtrCalculator,
    /// 当前趋势状态，随 K 线更新 MACD 后刷新
    trend: Option<Trend>,
    /// 上一次成交的参考价格，网格从此价格向两侧延伸
    last_fill_price: Option<f64>,
}

impl MacdGridStrategy {
    pub fn new(config: MacdGridConfig) -> Self {
        Self {
            config,
            macd_15m: MacdCalculator::new(),
            macd_1h: MacdCalculator::new(),
            macd_4h: MacdCalculator::new(),
            atr: AtrCalculator::new(14),
            trend: None,
            last_fill_price: None,
        }
    }

    fn symbol(&self) -> &Symbol {
        &self.config.symbol
    }

    fn grid_params(&self, trend: Trend) -> GridParams {
        let agg = self.config.aggressive_spacing_factor;
        let con = self.config.conservative_spacing_factor;

        match trend {
            Trend::StrongBull => GridParams {
                buy_base: agg,
                sell_base: con,
                max_long_usd: self.config.max_position_usd,
                max_short_usd: 0.0,
            },
            Trend::WeakBull => GridParams {
                buy_base: con,
                sell_base: con,
                max_long_usd: self.config.weak_position_usd,
                max_short_usd: 0.0,
            },
            Trend::WeakBear => GridParams {
                buy_base: con,
                sell_base: con,
                max_long_usd: 0.0,
                max_short_usd: self.config.weak_position_usd,
            },
            Trend::StrongBear => GridParams {
                buy_base: con,
                sell_base: agg,
                max_long_usd: 0.0,
                max_short_usd: self.config.max_position_usd,
            },
        }
    }

    fn deviation(&self, mid_price: f64, atr: f64) -> f64 {
        let ema = self
            .macd_15m
            .slow_ema_value()
            .expect("slow_ema must be ready when indicators_ready");
        (mid_price - ema) / atr
    }

    fn indicators_ready(&self) -> bool {
        self.macd_15m.is_ready()
            && self.macd_1h.is_ready()
            && self.macd_4h.is_ready()
            && self.atr.is_ready()
    }

    fn feed_candle(&mut self, close: f64, high: f64, low: f64, interval: CandleInterval, confirm: bool) {
        match interval {
            CandleInterval::Min15 => {
                if confirm {
                    self.macd_15m.update(close);
                    self.atr.update(high, low, close);
                } else {
                    self.macd_15m.update_live(close);
                    self.atr.update_live(high, low, close);
                }
            }
            CandleInterval::Hour1 => {
                if confirm {
                    self.macd_1h.update(close);
                } else {
                    self.macd_1h.update_live(close);
                }
            }
            CandleInterval::Hour4 => {
                if confirm {
                    self.macd_4h.update(close);
                } else {
                    self.macd_4h.update_live(close);
                }
            }
            _ => {}
        }
        self.update_trend();
    }

    /// 从 3 周期 MACD DEA 重新计算趋势，检测变化时重置网格参考价
    fn update_trend(&mut self) {
        let (Some(dea_15m), Some(dea_1h), Some(dea_4h)) =
            (self.macd_15m.dea(), self.macd_1h.dea(), self.macd_4h.dea())
        else {
            return;
        };

        let above_count = [dea_15m, dea_1h, dea_4h]
            .iter()
            .filter(|d| **d > 0.0)
            .count() as i8;

        // 15m bar 趋势微调: 连续递增 +1 (增强看涨), 连续递减 -1 (增强看跌)
        let bar_trend = self.macd_15m.bar_trend();
        let score = (above_count + bar_trend).clamp(0, 3);

        let new_trend = match score {
            3 => Trend::StrongBull,
            2 => Trend::WeakBull,
            1 => Trend::WeakBear,
            0 => Trend::StrongBear,
            _ => unreachable!(),
        };

        if self.trend != Some(new_trend) {
            if self.trend.is_some() {
                tracing::info!(
                    symbol = %self.symbol(),
                    old_trend = ?self.trend,
                    new_trend = ?new_trend,
                    "Trend changed, will reset grid reference price on next BBO"
                );
                // 趋势切换时清除参考价，下次 check_grid 会用 mid_price 重新初始化
                self.last_fill_price = None;
            }
            self.trend = Some(new_trend);
        }
    }

    fn pos_factor(&self, pos_usd: f64, max_pos_usd: f64) -> f64 {
        if max_pos_usd <= 0.0 {
            return 1.0;
        }
        1.0 + (pos_usd / max_pos_usd) * self.config.pos_weight
    }

    /// 网格核心逻辑：在网格价位挂 GTC 限价单，等待成交。
    /// 同时挂出减仓单和开仓单（如果条件允许），一次返回所有订单。
    fn check_grid(&mut self, state: &StateManager) -> Vec<OutcomeEvent> {
        let (Some(symbol_state), Some(trend), Some(atr)) =
            (state.symbol_state(self.symbol()), self.trend, self.atr.value())
        else {
            return vec![];
        };
        let Some(bbo) = symbol_state.bbo(Exchange::OKX) else {
            return vec![];
        };
        let mid_price = bbo.mid_price();

        if mid_price <= 0.0 || atr <= 0.0 {
            return vec![];
        }

        let params = self.grid_params(trend);
        let pos_size = symbol_state
            .position(Exchange::OKX)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let long_usd = pos_size.max(0.0) * mid_price;
        let short_usd = (-pos_size).max(0.0) * mid_price;

        // 超买超卖系数: deviation > 0 → 超买 → 买入更远、卖出更近
        let deviation = self.deviation(mid_price, atr);
        let w = self.config.ob_weight;
        let ob_buy_factor = (1.0 + deviation * w).clamp(0.5, 3.0);
        let ob_sell_factor = (1.0 - deviation * w).clamp(0.5, 3.0);

        // 仓位深度系数: 仅影响开仓方向，归一化到 [1, 1+pos_weight]
        let long_pos_factor = self.pos_factor(long_usd, params.max_long_usd);
        let short_pos_factor = self.pos_factor(short_usd, params.max_short_usd);

        let min = self.config.min_spacing;

        // 统一间距公式: spacing = max(ATR * base * ob_factor [* pos_factor], min_spacing)
        let reduce_buy_spacing = (atr * params.buy_base * ob_buy_factor).max(min);
        let open_buy_spacing = (atr * params.buy_base * ob_buy_factor * long_pos_factor).max(min);
        let reduce_sell_spacing = (atr * params.sell_base * ob_sell_factor).max(min);
        let open_sell_spacing = (atr * params.sell_base * ob_sell_factor * short_pos_factor).max(min);

        // 初始化网格参考价
        if self.last_fill_price.is_none() {
            self.last_fill_price = Some(mid_price);
            return vec![];
        }
        let mut last = self.last_fill_price.unwrap();

        // 追单: 持仓与趋势不一致或超限时，last_fill_price 跟随价格移动，
        // 确保平仓挂单价始终贴近当前价，避免反转后无法平掉反向仓位
        let need_reduce_long = pos_size > POSITION_EPSILON
            && (params.max_long_usd == 0.0 || long_usd > params.max_long_usd);
        let need_reduce_short = pos_size < -POSITION_EPSILON
            && (params.max_short_usd == 0.0 || short_usd > params.max_short_usd);

        if need_reduce_long && mid_price < last {
            self.last_fill_price = Some(mid_price);
            last = mid_price;
        }
        if need_reduce_short && mid_price > last {
            self.last_fill_price = Some(mid_price);
            last = mid_price;
        }

        // 同时收集减仓和开仓订单：减仓是保护性挂单（止盈/止损），
        // 开仓是网格推进，两者可以并存（如持多时同时挂卖出减仓 + 买入加仓）
        // 每个方向至多一笔挂单，已有同方向挂单时跳过
        let has_pending_long = symbol_state.has_pending_side(Side::Long);
        let has_pending_short = symbol_state.has_pending_side(Side::Short);

        let mut orders = Vec::new();
        let mut comments = Vec::new();

        // === 减仓 ===
        // 有多头仓位 → 卖出减仓
        if pos_size > POSITION_EPSILON && !has_pending_short {
            let grid_price = last + reduce_sell_spacing;
            let qty = (self.config.order_usd_value / grid_price).min(pos_size);
            if let Some(order) = self.make_order(
                Side::Short, grid_price, qty, true,
                &format!("grid_reduce_long | trend={:?} | dev={:.2} | ob={:.2} | spacing={:.4}",
                    trend, deviation, ob_sell_factor, reduce_sell_spacing),
            ) {
                orders.push(order);
                comments.push(format!("reduce_long@{:.4}", grid_price));
            }
        }
        // 有空头仓位 → 买入减仓
        if pos_size < -POSITION_EPSILON && !has_pending_long {
            let grid_price = last - reduce_buy_spacing;
            let qty = (self.config.order_usd_value / grid_price).min(pos_size.abs());
            if let Some(order) = self.make_order(
                Side::Long, grid_price, qty, true,
                &format!("grid_reduce_short | trend={:?} | dev={:.2} | ob={:.2} | spacing={:.4}",
                    trend, deviation, ob_buy_factor, reduce_buy_spacing),
            ) {
                orders.push(order);
                comments.push(format!("reduce_short@{:.4}", grid_price));
            }
        }

        // === 开仓 ===
        // 允许开多且未超限 → 买入开多
        if params.max_long_usd > 0.0 && long_usd < params.max_long_usd && !has_pending_long {
            let grid_price = last - open_buy_spacing;
            let qty = self.config.order_usd_value / grid_price;
            if let Some(order) = self.make_order(
                Side::Long, grid_price, qty, false,
                &format!("grid_open_long | trend={:?} | dev={:.2} | ob={:.2} | pos_f={:.1} | spacing={:.4}",
                    trend, deviation, ob_buy_factor, long_pos_factor, open_buy_spacing),
            ) {
                orders.push(order);
                comments.push(format!("open_long@{:.4}", grid_price));
            }
        }
        // 允许开空且未超限 → 卖出开空
        if params.max_short_usd > 0.0 && short_usd < params.max_short_usd && !has_pending_short {
            let grid_price = last + open_sell_spacing;
            let qty = self.config.order_usd_value / grid_price;
            if let Some(order) = self.make_order(
                Side::Short, grid_price, qty, false,
                &format!("grid_open_short | trend={:?} | dev={:.2} | ob={:.2} | pos_f={:.1} | spacing={:.4}",
                    trend, deviation, ob_sell_factor, short_pos_factor, open_sell_spacing),
            ) {
                orders.push(order);
                comments.push(format!("open_short@{:.4}", grid_price));
            }
        }

        if orders.is_empty() {
            return vec![];
        }

        vec![OutcomeEvent::PlaceOrders {
            comment: format!("grid | trend={:?} | {}", trend, comments.join(" + ")),
            orders,
        }]
    }

    fn make_order(
        &self,
        side: Side,
        price: f64,
        qty: f64,
        reduce_only: bool,
        comment: &str,
    ) -> Option<Order> {
        if qty <= 0.0 || price <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol(),
                side = ?side,
                price, qty,
                "Grid make_order skipped: invalid price or qty"
            );
            return None;
        }

        tracing::info!(
            symbol = %self.symbol(),
            side = ?side,
            price = format!("{:.4}", price),
            qty = format!("{:.6}", qty),
            reduce_only,
            comment,
            "Placing grid order"
        );

        Some(Order {
            id: String::new(),
            exchange: Exchange::OKX,
            symbol: self.symbol().clone(),
            side,
            order_type: OrderType::Limit {
                price,
                tif: TimeInForce::GTC,
            },
            quantity: qty,
            reduce_only,
            client_order_id: String::new(),
        })
    }

    fn log_status(&self, state: &StateManager) {
        let symbol_state = match state.symbol_state(self.symbol()) {
            Some(s) => s,
            None => return,
        };

        let mid_price = symbol_state
            .bbo(Exchange::OKX)
            .map(|b| b.mid_price());
        let pos = symbol_state
            .position(Exchange::OKX)
            .map(|p| p.size);

        let atr = self.atr.value();
        let deviation = match (mid_price, atr, self.macd_15m.slow_ema_value()) {
            (Some(mid), Some(a), Some(_)) if a > 0.0 => {
                let ema = self.macd_15m.slow_ema_value().unwrap();
                Some((mid - ema) / a)
            }
            _ => None,
        };

        tracing::info!(
            symbol = %self.symbol(),
            trend = ?self.trend,
            ready = self.indicators_ready(),
            mid_price = mid_price.map(|p| format!("{:.4}", p)),
            pos = pos.map(|p| format!("{:.6}", p)),
            atr = atr.map(|a| format!("{:.4}", a)),
            slow_ema_15m = self.macd_15m.slow_ema_value().map(|v| format!("{:.4}", v)),
            dif_15m = self.macd_15m.macd_line().map(|d| format!("{:.6}", d)),
            dea_15m = self.macd_15m.dea().map(|d| format!("{:.6}", d)),
            bar_15m = self.macd_15m.bar().map(|b| format!("{:.6}", b)),
            bar_trend_15m = self.macd_15m.bar_trend(),
            dif_1h = self.macd_1h.macd_line().map(|d| format!("{:.6}", d)),
            dea_1h = self.macd_1h.dea().map(|d| format!("{:.6}", d)),
            dif_4h = self.macd_4h.macd_line().map(|d| format!("{:.6}", d)),
            dea_4h = self.macd_4h.dea().map(|d| format!("{:.6}", d)),
            deviation = deviation.map(|d| format!("{:.4}", d)),
            last_fill_price = self.last_fill_price.map(|p| format!("{:.4}", p)),
            "MacdGrid status"
        );
    }
}

impl Strategy for MacdGridStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let kinds: HashSet<SubscriptionKind> = [
            SubscriptionKind::BBO {
                symbol: self.symbol().clone(),
            },
            SubscriptionKind::Candle {
                symbol: self.symbol().clone(),
                interval: CandleInterval::Min15,
            },
            SubscriptionKind::Candle {
                symbol: self.symbol().clone(),
                interval: CandleInterval::Hour1,
            },
            SubscriptionKind::Candle {
                symbol: self.symbol().clone(),
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

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent> {
        match &event.data {
            ExchangeEventData::HistoryCandles(candles) => {
                for candle in candles {
                    if candle.symbol == *self.symbol() {
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
                    symbol = %self.symbol(),
                    count = candles.len(),
                    interval = candles.first().map(|c| c.interval.to_string()).unwrap_or_default(),
                    ready = self.indicators_ready(),
                    "Fed history candles"
                );
                vec![]
            }
            ExchangeEventData::Candle(candle) => {
                if candle.symbol == *self.symbol() {
                    self.feed_candle(
                        candle.close,
                        candle.high,
                        candle.low,
                        candle.interval,
                        candle.confirm,
                    );
                }
                vec![]
            }
            ExchangeEventData::Clock => {
                self.log_status(state);
                vec![]
            }
            ExchangeEventData::Fill(fill) => {
                if fill.symbol == *self.symbol() {
                    tracing::info!(
                        symbol = %self.symbol(),
                        side = ?fill.side,
                        fill_price = format!("{:.4}", fill.price),
                        fill_size = format!("{:.6}", fill.size),
                        "Grid order filled, updating reference price"
                    );
                    self.last_fill_price = Some(fill.price);
                }
                vec![]
            }
            ExchangeEventData::BBO(_) => {
                if self.trend.is_none() || !self.indicators_ready() {
                    return vec![];
                }
                self.check_grid(state)
            }
            _ => vec![],
        }
    }
}

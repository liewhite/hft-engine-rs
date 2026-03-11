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
    /// 上一次下单的参考价格，网格从此价格向两侧延伸
    last_order_price: Option<f64>,
    /// 上一次的趋势状态，用于检测趋势切换并重置网格参考价
    last_trend: Option<Trend>,
}

impl MacdGridStrategy {
    pub fn new(config: MacdGridConfig) -> Self {
        Self {
            config,
            macd_15m: MacdCalculator::new(),
            macd_1h: MacdCalculator::new(),
            macd_4h: MacdCalculator::new(),
            atr: AtrCalculator::new(14),
            last_order_price: None,
            last_trend: None,
        }
    }

    fn symbol(&self) -> &Symbol {
        &self.config.symbol
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

    /// 超买超卖偏离度: (mid_price - slow_ema_15m) / ATR
    /// 正值 = 超买, 负值 = 超卖
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

    /// 仓位深度系数: 1 + (pos_usd / max_pos_usd) * pos_weight
    /// 归一化到 [1, 1+pos_weight]，与 ob_factor 尺度对等
    fn pos_factor(&self, pos_usd: f64, max_pos_usd: f64) -> f64 {
        if max_pos_usd <= 0.0 {
            return 1.0;
        }
        1.0 + (pos_usd / max_pos_usd) * self.config.pos_weight
    }

    /// 网格核心逻辑：检查价格是否穿越网格线，触发则下一笔 IOC 订单。
    /// 单个 BBO tick 最多触发一笔订单，last_order_price 每次只移动一个网格步长，
    /// 配合 has_pending_orders 保护，确保不会在短时间内连续下单。
    fn check_grid(&mut self, state: &StateManager) -> Option<OutcomeEvent> {
        let symbol_state = state.symbol_state(self.symbol())?;
        let trend = self.current_trend()?;
        let atr = self.atr.value()?;
        let bbo = symbol_state.bbo(Exchange::OKX)?;
        let mid_price = bbo.mid_price();

        if mid_price <= 0.0 || atr <= 0.0 {
            return None;
        }

        // 趋势切换时重置网格参考价，防止 last_order_price 远离当前价导致连续触发
        if self.last_trend != Some(trend) {
            if self.last_trend.is_some() {
                tracing::info!(
                    symbol = %self.symbol(),
                    old_trend = ?self.last_trend,
                    new_trend = ?trend,
                    mid_price = format!("{:.4}", mid_price),
                    "Trend changed, resetting grid reference price"
                );
                self.last_order_price = Some(mid_price);
            }
            self.last_trend = Some(trend);
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
        if self.last_order_price.is_none() {
            self.last_order_price = Some(mid_price);
            return None;
        }
        let last = self.last_order_price.unwrap();

        // === 买入方向 ===
        // 优先: 有空头仓位 → 买入减仓
        if mid_price <= last - reduce_buy_spacing && pos_size < -POSITION_EPSILON {
            let qty = (self.config.order_usd_value / bbo.ask_price).min(pos_size.abs());
            self.last_order_price = Some(last - reduce_buy_spacing);
            return self.make_order(
                Side::Long,
                bbo.ask_price,
                qty,
                true,
                &format!(
                    "grid_reduce_short | trend={:?} | dev={:.2} | ob={:.2} | spacing={:.4}",
                    trend, deviation, ob_buy_factor, reduce_buy_spacing
                ),
            );
        }
        // 其次: 允许开多且未超限 → 买入开多
        if mid_price <= last - open_buy_spacing
            && params.max_long_usd > 0.0
            && long_usd < params.max_long_usd
        {
            let qty = self.config.order_usd_value / bbo.ask_price;
            self.last_order_price = Some(last - open_buy_spacing);
            return self.make_order(
                Side::Long,
                bbo.ask_price,
                qty,
                false,
                &format!(
                    "grid_open_long | trend={:?} | dev={:.2} | ob={:.2} | pos_f={:.1} | spacing={:.4}",
                    trend, deviation, ob_buy_factor, long_pos_factor, open_buy_spacing
                ),
            );
        }

        // === 卖出方向 ===
        // 优先: 有多头仓位 → 卖出减仓
        if mid_price >= last + reduce_sell_spacing && pos_size > POSITION_EPSILON {
            let qty = (self.config.order_usd_value / bbo.bid_price).min(pos_size);
            self.last_order_price = Some(last + reduce_sell_spacing);
            return self.make_order(
                Side::Short,
                bbo.bid_price,
                qty,
                true,
                &format!(
                    "grid_reduce_long | trend={:?} | dev={:.2} | ob={:.2} | spacing={:.4}",
                    trend, deviation, ob_sell_factor, reduce_sell_spacing
                ),
            );
        }
        // 其次: 允许开空且未超限 → 卖出开空
        if mid_price >= last + open_sell_spacing
            && params.max_short_usd > 0.0
            && short_usd < params.max_short_usd
        {
            let qty = self.config.order_usd_value / bbo.bid_price;
            self.last_order_price = Some(last + open_sell_spacing);
            return self.make_order(
                Side::Short,
                bbo.bid_price,
                qty,
                false,
                &format!(
                    "grid_open_short | trend={:?} | dev={:.2} | ob={:.2} | pos_f={:.1} | spacing={:.4}",
                    trend, deviation, ob_sell_factor, short_pos_factor, open_sell_spacing
                ),
            );
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
            symbol = %self.symbol(),
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
                symbol: self.symbol().clone(),
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
        let symbol_state = match state.symbol_state(self.symbol()) {
            Some(s) => s,
            None => return,
        };

        let mid = symbol_state
            .bbo(Exchange::OKX)
            .map(|b| format!("{:.4}", b.mid_price()));
        let pos = symbol_state
            .position(Exchange::OKX)
            .map(|p| format!("{:.4}", p.size));

        tracing::info!(
            symbol = %self.symbol(),
            trend = ?self.current_trend(),
            dea_15m = self.macd_15m.dea().map(|d| format!("{:.6}", d)),
            dea_1h = self.macd_1h.dea().map(|d| format!("{:.6}", d)),
            dea_4h = self.macd_4h.dea().map(|d| format!("{:.6}", d)),
            atr = self.atr.value().map(|a| format!("{:.4}", a)),
            mid_price = mid,
            pos = pos,
            last_order_price = self.last_order_price.map(|p| format!("{:.4}", p)),
            ready = self.indicators_ready(),
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

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Option<OutcomeEvent> {
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
                None
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
                None
            }
            ExchangeEventData::Clock => {
                self.log_status(state);
                None
            }
            ExchangeEventData::BBO(_) => {
                if !self.indicators_ready() {
                    return None;
                }
                let symbol_state = state.symbol_state(self.symbol())?;
                if symbol_state.has_pending_orders() {
                    return None;
                }
                self.check_grid(state)
            }
            _ => None,
        }
    }
}

use crate::domain::{
    Exchange, Order, OrderType, Price, Quantity, Rate, Side, Symbol, TimeInForce, BBO,
};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager, SymbolState};
use crate::strategy::{OutcomeEvent, Strategy};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// 最大允许的时间戳差异（毫秒）
const MAX_TIMESTAMP_DIFF_MS: u64 = 60_000; // 1 分钟

/// EMA (Exponential Moving Average) 计算器
#[derive(Debug, Clone)]
pub struct EmaCalculator {
    period: usize,
    alpha: f64,
    value: Option<f64>,
    count: usize,
}

impl EmaCalculator {
    pub fn new(period: usize) -> Self {
        let alpha = 2.0 / (period as f64 + 1.0);
        Self {
            period,
            alpha,
            value: None,
            count: 0,
        }
    }

    /// 更新 EMA，返回当前 EMA 值
    pub fn update(&mut self, new_value: f64) -> f64 {
        self.count += 1;
        match self.value {
            None => {
                self.value = Some(new_value);
                new_value
            }
            Some(prev) => {
                let ema = self.alpha * new_value + (1.0 - self.alpha) * prev;
                self.value = Some(ema);
                ema
            }
        }
    }

    /// 获取当前 EMA 值
    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// 获取更新次数
    pub fn count(&self) -> usize {
        self.count
    }

    /// 是否已经预热完成（满足 period 次更新）
    pub fn is_ready(&self) -> bool {
        self.count >= self.period
    }
}

/// 资金费率套利策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct FundingArbConfig {
    /// EMA 周期（默认 100）
    #[serde(default = "default_ema_period")]
    pub ema_period: usize,
    /// 开仓偏离阈值（metric 偏离 EMA 的百分比，如 0.003 表示 0.3%）
    #[serde(default = "default_open_deviation")]
    pub open_deviation: f64,
    /// 平仓偏离阈值（metric 回归 EMA 的百分比，如 0.0005 表示 0.05%）
    #[serde(default = "default_close_deviation")]
    pub close_deviation: f64,
    /// 最小资费差（日化，如 0.003 表示 0.3%）
    /// 只有当最大资费日化 - 最小资费日化 > 此阈值时才允许开仓
    #[serde(default = "default_min_funding_spread")]
    pub min_funding_spread: Rate,
    /// 单笔最大下单金额 (USDT)
    #[serde(default = "default_max_notional")]
    pub max_notional: f64,
    /// 最大持仓数量
    #[serde(default = "default_max_quantity")]
    pub max_quantity: Quantity,
    /// 订单超时时间 (毫秒)
    #[serde(default = "default_order_timeout_ms")]
    pub order_timeout_ms: u64,
    /// 单交易所仓位/净值比例限制（如 0.2 表示 20%）
    #[serde(default = "default_max_position_equity_ratio")]
    pub max_position_equity_ratio: f64,
    /// 敞口比例限制（敞口/较小仓位，如 0.01 表示 1%）
    #[serde(default = "default_max_exposure_ratio")]
    pub max_exposure_ratio: f64,
}

fn default_ema_period() -> usize {
    100
}
fn default_open_deviation() -> f64 {
    0.003 // 0.3%
}
fn default_close_deviation() -> f64 {
    0.0005 // 0.05%
}
fn default_min_funding_spread() -> Rate {
    0.003 // 0.3% 日化资费差
}
fn default_max_notional() -> f64 {
    1000.0
}
fn default_max_quantity() -> Quantity {
    1.0
}
fn default_order_timeout_ms() -> u64 {
    10_000
}
fn default_max_position_equity_ratio() -> f64 {
    0.2 // 20%
}
fn default_max_exposure_ratio() -> f64 {
    0.01 // 1%
}

impl Default for FundingArbConfig {
    fn default() -> Self {
        Self {
            ema_period: default_ema_period(),
            open_deviation: default_open_deviation(),
            close_deviation: default_close_deviation(),
            min_funding_spread: default_min_funding_spread(),
            max_notional: default_max_notional(),
            max_quantity: default_max_quantity(),
            order_timeout_ms: default_order_timeout_ms(),
            max_position_equity_ratio: default_max_position_equity_ratio(),
            max_exposure_ratio: default_max_exposure_ratio(),
        }
    }
}

/// Metric 计算结果
#[derive(Debug, Clone)]
struct MetricResult {
    /// metric = short_bid / long_ask - 1
    metric: f64,
    /// 资费最高交易所（做空）
    short_exchange: Exchange,
    /// 资费最高交易所的 BBO
    short_bbo: BBO,
    /// 资费最低交易所（做多）
    long_exchange: Exchange,
    /// 资费最低交易所的 BBO
    long_bbo: BBO,
}

/// 资金费率套利策略 (单 symbol)
///
/// 策略逻辑：
/// 1. 监控 metric = 最大资费bid1 / 最小资费ask1 - 1
/// 2. 维护 metric 的 EMA，在 BBO 事件时更新
/// 3. 满 ema_period 次更新后才允许开平仓
/// 4. 当 metric 偏离 EMA > open_deviation 时开仓
/// 5. 当 metric 回归 EMA < close_deviation 时平仓
pub struct FundingArbStrategy {
    config: FundingArbConfig,
    exchanges: Vec<Exchange>,
    symbol: Symbol,
    /// EMA 计算器
    ema: EmaCalculator,
    /// 当前 metric 值
    current_metric: Option<f64>,
}

impl FundingArbStrategy {
    pub fn new(config: FundingArbConfig, exchanges: Vec<Exchange>, symbol: Symbol) -> Self {
        let ema = EmaCalculator::new(config.ema_period);
        Self {
            config,
            exchanges,
            symbol,
            ema,
            current_metric: None,
        }
    }

    /// 计算基于剩余时间的资费差（日化）
    ///
    /// 使用各交易所 FundingRate 中最小的时间戳作为参考时间
    /// 如果最大时间戳 - 最小时间戳 > 1分钟，返回 None 并打印警告
    ///
    /// 返回 (最大日化资费, 最小日化资费, 资费差)
    fn calculate_funding_spread(&self, state: &SymbolState) -> Option<(Rate, Rate, Rate)> {
        let (short_ex, short_rate) = state.best_short_exchange()?;
        let (long_ex, long_rate) = state.best_long_exchange()?;

        // 检查时间戳差异
        let min_ts = short_rate.timestamp.min(long_rate.timestamp);
        let max_ts = short_rate.timestamp.max(long_rate.timestamp);
        let ts_diff = max_ts - min_ts;

        if ts_diff > MAX_TIMESTAMP_DIFF_MS {
            tracing::warn!(
                symbol = %self.symbol,
                short_exchange = %short_ex,
                short_ts = short_rate.timestamp,
                long_exchange = %long_ex,
                long_ts = long_rate.timestamp,
                ts_diff_ms = ts_diff,
                max_allowed_ms = MAX_TIMESTAMP_DIFF_MS,
                "Funding rate timestamp difference too large, refusing to open"
            );
            return None;
        }

        // 使用最小时间戳作为参考时间
        let reference_time = min_ts;

        let short_daily = short_rate.daily_rate_by_time_remaining(reference_time);
        let long_daily = long_rate.daily_rate_by_time_remaining(reference_time);

        let spread = short_daily - long_daily;

        tracing::debug!(
            symbol = %self.symbol,
            short_exchange = %short_ex,
            short_rate = format!("{:.6}", short_rate.rate),
            short_daily = format!("{:.6}", short_daily),
            long_exchange = %long_ex,
            long_rate = format!("{:.6}", long_rate.rate),
            long_daily = format!("{:.6}", long_daily),
            funding_spread = format!("{:.6}", spread),
            reference_time = reference_time,
            "Funding spread calculated"
        );

        Some((short_daily, long_daily, spread))
    }

    /// 计算 metric = short_bid / long_ask - 1
    ///
    /// short_exchange: 资费最高的交易所（适合做空）
    /// long_exchange: 资费最低的交易所（适合做多）
    fn calculate_metric(state: &SymbolState) -> Option<MetricResult> {
        let (short_exchange, _) = state.best_short_exchange()?;
        let (long_exchange, _) = state.best_long_exchange()?;

        let short_bbo = state.bbo(short_exchange)?.clone();
        let long_bbo = state.bbo(long_exchange)?.clone();

        if long_bbo.ask_price <= 0.0 {
            return None;
        }

        let metric = short_bbo.bid_price / long_bbo.ask_price - 1.0;

        Some(MetricResult {
            metric,
            short_exchange,
            short_bbo,
            long_exchange,
            long_bbo,
        })
    }

    /// 检查开仓条件
    ///
    /// 条件：
    /// 1. 无未完成订单
    /// 2. 资费差（日化）> min_funding_spread
    /// 3. metric 偏离 EMA > open_deviation
    /// 4. 风控检查通过
    ///
    /// 前置条件：EMA 已预热完成（由 on_event 保证）
    fn check_open_condition(
        &self,
        state: &SymbolState,
        metric_result: &MetricResult,
        state_manager: &StateManager,
    ) -> bool {
        // 有未完成订单
        if state.has_pending_orders() {
            return false;
        }

        // 检查资费差（基于剩余时间的日化）
        let funding_spread = match self.calculate_funding_spread(state) {
            Some((_, _, spread)) => spread,
            None => return false,
        };

        if funding_spread < self.config.min_funding_spread {
            tracing::debug!(
                symbol = %self.symbol,
                funding_spread = format!("{:.6}", funding_spread),
                min_funding_spread = format!("{:.6}", self.config.min_funding_spread),
                "Funding spread too low for opening"
            );
            return false;
        }

        // is_ready() == true 保证 value() 为 Some
        let ema_value = self.ema.value()
            .expect("EMA value must exist when is_ready() is true");

        // 计算偏离度 = metric - ema
        let deviation = metric_result.metric - ema_value;

        // 偏离度 > open_deviation 时开仓
        // metric > ema 意味着 short_bid/long_ask 比均值高，适合做空 short_exchange、做多 long_exchange
        if deviation.abs() < self.config.open_deviation {
            return false;
        }

        // 风控检查：单交易所仓位限制
        let short_equity = state_manager.equity(metric_result.short_exchange);
        let long_equity = state_manager.equity(metric_result.long_exchange);

        if short_equity <= 0.0 || long_equity <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                short_equity = short_equity,
                long_equity = long_equity,
                "Insufficient equity"
            );
            return false;
        }

        tracing::info!(
            symbol = %self.symbol,
            metric = format!("{:.6}", metric_result.metric),
            ema = format!("{:.6}", ema_value),
            deviation = format!("{:.6}", deviation),
            funding_spread = format!("{:.6}", funding_spread),
            short_exchange = %metric_result.short_exchange,
            long_exchange = %metric_result.long_exchange,
            "Opening condition met"
        );

        true
    }

    /// 检查平仓条件
    ///
    /// 条件：
    /// 1. 有持仓
    /// 2. metric 回归 EMA（偏离度 < close_deviation）
    ///
    /// 前置条件：EMA 已预热完成且 current_metric 已设置（开仓前提条件保证）
    fn check_close_condition(&self, state: &SymbolState) -> bool {
        if !state.has_positions() {
            return false;
        }

        // 有持仓时，EMA 和 metric 必然存在（开仓前提条件）
        let ema_value = self.ema.value()
            .expect("EMA must exist when positions are open");
        let metric = self.current_metric
            .expect("current_metric must exist when positions are open");

        let deviation = (metric - ema_value).abs();

        if deviation < self.config.close_deviation {
            tracing::info!(
                symbol = %self.symbol,
                metric = format!("{:.6}", metric),
                ema = format!("{:.6}", ema_value),
                deviation = format!("{:.6}", deviation),
                "Closing condition met - metric reverted to EMA"
            );
            return true;
        }

        false
    }

    /// 检查敞口是否在允许范围内
    ///
    /// 敞口限制：敞口 < min(|long|, |short|) * max_exposure_ratio
    fn check_exposure(&self, state: &SymbolState) -> bool {
        let (long_size, short_size) = state.position_sizes();

        if long_size.abs() < 1e-10 && short_size.abs() < 1e-10 {
            return true; // 无持仓，无敞口问题
        }

        let exposure = (long_size + short_size).abs();
        let min_position = long_size.abs().min(short_size.abs());

        if min_position < 1e-10 {
            // 只有单边持仓，允许（可能是刚开仓）
            return true;
        }

        let exposure_ratio = exposure / min_position;

        if exposure_ratio > self.config.max_exposure_ratio {
            tracing::warn!(
                symbol = %self.symbol,
                long_size = long_size,
                short_size = short_size,
                exposure = exposure,
                exposure_ratio = format!("{:.4}", exposure_ratio),
                max_ratio = self.config.max_exposure_ratio,
                "Exposure exceeds limit"
            );
            return false;
        }

        true
    }

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
    fn make_open_orders(
        &self,
        metric_result: &MetricResult,
        state_manager: &StateManager,
    ) -> Vec<Order> {
        let short_equity = state_manager.equity(metric_result.short_exchange);
        let long_equity = state_manager.equity(metric_result.long_exchange);

        // 取两边净值较小的那个，再乘以仓位比例限制
        let min_equity = short_equity.min(long_equity);
        let max_position_value = min_equity * self.config.max_position_equity_ratio;

        if max_position_value <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                short_equity = short_equity,
                long_equity = long_equity,
                "Insufficient equity for opening position"
            );
            return vec![];
        }

        let short_price = metric_result.short_bbo.bid_price;
        let short_qty = Self::calculate_quantity(
            &self.config,
            short_price,
            metric_result.short_bbo.bid_qty,
            max_position_value,
        );

        let long_price = metric_result.long_bbo.ask_price;
        let long_qty = Self::calculate_quantity(
            &self.config,
            long_price,
            metric_result.long_bbo.ask_qty,
            max_position_value,
        );

        let qty = short_qty.min(long_qty);

        if qty <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                short_qty = short_qty,
                long_qty = long_qty,
                "Calculated quantity is zero or negative"
            );
            return vec![];
        }

        tracing::info!(
            symbol = %self.symbol,
            short_ex = %metric_result.short_exchange,
            short_price = short_price,
            long_ex = %metric_result.long_exchange,
            long_price = long_price,
            qty = qty,
            "Opening positions"
        );

        vec![
            Order {
                id: String::new(),
                exchange: metric_result.short_exchange,
                symbol: self.symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Limit {
                    price: short_price,
                    tif: TimeInForce::IOC,
                },
                quantity: qty,
                reduce_only: false,
                client_order_id: String::new(),
            },
            Order {
                id: String::new(),
                exchange: metric_result.long_exchange,
                symbol: self.symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price: long_price,
                    tif: TimeInForce::IOC,
                },
                quantity: qty,
                reduce_only: false,
                client_order_id: String::new(),
            },
        ]
    }

    /// 生成平仓订单
    fn make_close_orders(&self, state: &SymbolState) -> Vec<Order> {
        let mut orders = Vec::new();

        for (exchange, pos) in &state.positions {
            if let Some(pos_side) = pos.side() {
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
                    symbol: self.symbol.clone(),
                    side: pos_side.opposite(),
                    order_type,
                    quantity: pos.size.abs(),
                    reduce_only: true,
                    client_order_id: String::new(),
                });
            }
        }

        orders
    }

    /// 打印市场指标
    fn log_market_metrics(&self, metric_result: Option<&MetricResult>) {
        let (metric, ema, deviation, ready) = match (metric_result, self.ema.value()) {
            (Some(mr), Some(ema)) => {
                let deviation = mr.metric - ema;
                (mr.metric, ema, deviation, self.ema.is_ready())
            }
            (Some(mr), None) => (mr.metric, 0.0, 0.0, false),
            _ => return,
        };

        tracing::info!(
            symbol = %self.symbol,
            metric = format!("{:.6}", metric),
            ema = format!("{:.6}", ema),
            deviation = format!("{:.6}", deviation),
            ema_count = self.ema.count(),
            ema_ready = ready,
            "Market metrics"
        );
    }
}

impl Strategy for FundingArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let kinds: HashSet<SubscriptionKind> = [
            SubscriptionKind::FundingRate {
                symbol: self.symbol.clone(),
            },
            SubscriptionKind::BBO {
                symbol: self.symbol.clone(),
            },
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

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent> {
        // 获取本策略关注的 symbol 状态
        let symbol_state = match state.symbol_state(&self.symbol) {
            Some(s) => s,
            None => return vec![],
        };

        // 计算 metric
        let metric_result = Self::calculate_metric(symbol_state);

        // BBO 事件时更新 EMA
        if matches!(&event.data, ExchangeEventData::BBO(_)) {
            if let Some(ref mr) = metric_result {
                self.ema.update(mr.metric);
                self.current_metric = Some(mr.metric);
            }
        }

        // 打印市场指标
        self.log_market_metrics(metric_result.as_ref());

        // EMA 未预热完成，不进行交易
        if !self.ema.is_ready() {
            return vec![];
        }

        // 有未完成订单时等待
        if symbol_state.has_pending_orders() {
            return vec![];
        }

        // 检查敞口
        if !self.check_exposure(symbol_state) {
            // 敞口超限，不操作（可以考虑后续添加修复逻辑）
            return vec![];
        }

        // 优先级 1: 检查平仓条件
        if self.check_close_condition(symbol_state) {
            let orders = self.make_close_orders(symbol_state);
            return orders.into_iter().map(OutcomeEvent::PlaceOrder).collect();
        }

        // 优先级 2: 检查开仓条件
        if let Some(ref mr) = metric_result {
            if self.check_open_condition(symbol_state, mr, state) {
                let orders = self.make_open_orders(mr, state);
                return orders.into_iter().map(OutcomeEvent::PlaceOrder).collect();
            }
        }

        vec![]
    }
}

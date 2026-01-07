use crate::domain::{Exchange, Order, OrderType, Rate, Side, Symbol, TimeInForce, BBO};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager, SymbolState};
use crate::strategy::{OutcomeEvent, Strategy};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// 最大允许的时间戳差异（毫秒）
const MAX_TIMESTAMP_DIFF_MS: u64 = 120_000; // 2 分钟

/// 市价单滑点（用限价单 IOC 模拟市价单）
const MARKET_ORDER_SLIPPAGE: f64 = 0.001; // 0.1%

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

    /// 是否已经预热完成（满足 period 次更新）
    pub fn is_ready(&self) -> bool {
        self.count >= self.period
    }
}

/// 资金费率套利策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct FundingArbConfig {
    /// EMA 周期
    pub ema_period: usize,
    /// 单笔下单金额 (USDT)，开平仓均按此金额计算数量
    pub max_notional: f64,
    /// 最小下单金额 (USDT)，低于此金额的订单将被放弃
    pub min_notional: f64,
    /// 订单超时时间 (毫秒)
    pub order_timeout_ms: u64,
    /// 敞口比例限制（敞口/较小仓位）
    /// - 超过此比例时禁止开仓
    /// - 配合 max_exposure_value 触发 rebalance
    pub max_exposure_ratio: f64,
    /// 敞口价值限制 (USDT)
    /// - 需同时超过 max_exposure_ratio 和 max_exposure_value 才触发 rebalance
    /// - 避免基础仓位小时频繁 rebalance
    pub max_exposure_value: f64,
    /// 单边仓位占账户 equity 的最大比例
    /// - 任一交易所的仓位价值 / equity 超过此比例时禁止开仓
    /// - 不影响平仓和 rebalance
    pub max_position_ratio: f64,
}

/// 根据资费差（日化）计算开平仓阈值
///
/// | 资费差日化 | 开仓阈值 | 平仓阈值 |
/// |-----------|---------|---------|
/// | < 0.2%    | 不开仓   | -       |
/// | 0.2%~0.3% | 0.20%   | -0.10%  |
/// | 0.3%~0.5% | 0.15%   | -0.15%  |
/// | 0.5%~0.8% | 0.10%   | -0.20%  |
/// | > 0.8%    | 0.05%   | -0.25%  |
///
/// 返回 (open_threshold, close_threshold)，None 表示不允许开仓
fn calculate_thresholds(funding_spread: Rate) -> Option<(f64, f64)> {
    if funding_spread < 0.002 {
        // < 0.2%，不开仓
        None
    } else if funding_spread < 0.003 {
        // 0.2% ~ 0.3%
        Some((0.002, -0.001))
    } else if funding_spread < 0.005 {
        // 0.3% ~ 0.5%
        Some((0.0015, -0.0015))
    } else if funding_spread < 0.008 {
        // 0.5% ~ 0.8%
        Some((0.001, -0.002))
    } else {
        // > 0.8%
        Some((0.0005, -0.0025))
    }
}

/// 开仓信号
#[derive(Debug, Clone)]
struct OpenSignal {
    /// 资费最高交易所（做空）
    short_exchange: Exchange,
    /// 做空价格
    short_price: f64,
    /// 资费最低交易所（做多）
    long_exchange: Exchange,
    /// 做多价格
    long_price: f64,
}

/// 平仓信号
#[derive(Debug, Clone)]
struct CloseSignal {
    /// 平多交易所
    long_exchange: Exchange,
    /// 平多价格（bid）
    long_price: f64,
    /// 平多交易所的持仓量
    long_size: f64,
    /// 平空交易所
    short_exchange: Exchange,
    /// 平空价格（ask）
    short_price: f64,
    /// 平空交易所的持仓量（负数）
    short_size: f64,
}

/// 交易所的 BBO EMA
#[derive(Debug, Clone)]
struct ExchangeEma {
    /// bid 价格 EMA（用于卖出：开空/平多）
    bid_ema: EmaCalculator,
    /// ask 价格 EMA（用于买入：开多/平空）
    ask_ema: EmaCalculator,
}

/// 资金费率套利策略 (单 symbol)
///
/// 策略逻辑：
/// 1. 为每个交易所维护独立的 bid_ema 和 ask_ema
/// 2. 开仓：资费最高交易所 bid 向上偏离 bid_ema + 资费最低交易所 ask 向下偏离 ask_ema ≥ 阈值
/// 3. 平仓：多头组中 bid 向上偏离最大 + 空头组中 ask 向下偏离最大 ≥ 阈值
pub struct FundingArbStrategy {
    config: FundingArbConfig,
    exchanges: Vec<Exchange>,
    symbol: Symbol,
    /// 每个交易所的 BBO EMA
    exchange_emas: HashMap<Exchange, ExchangeEma>,
}

impl FundingArbStrategy {
    pub fn new(config: FundingArbConfig, exchanges: Vec<Exchange>, symbol: Symbol) -> Self {
        let exchange_emas = exchanges
            .iter()
            .map(|ex| {
                (
                    *ex,
                    ExchangeEma {
                        bid_ema: EmaCalculator::new(config.ema_period),
                        ask_ema: EmaCalculator::new(config.ema_period),
                    },
                )
            })
            .collect();
        Self {
            config,
            exchanges,
            symbol,
            exchange_emas,
        }
    }

    /// 更新某交易所的 EMA
    fn update_ema(&mut self, exchange: Exchange, bbo: &BBO) {
        if let Some(ema) = self.exchange_emas.get_mut(&exchange) {
            if bbo.bid_price > 0.0 {
                ema.bid_ema.update(bbo.bid_price);
            }
            if bbo.ask_price > 0.0 {
                ema.ask_ema.update(bbo.ask_price);
            }
        }
    }

    /// 检查所有交易所的 EMA 是否都预热完成
    fn all_emas_ready(&self) -> bool {
        self.exchange_emas
            .values()
            .all(|ema| ema.bid_ema.is_ready() && ema.ask_ema.is_ready())
    }

    /// 计算某交易所 bid 的向上偏离（正值表示向上偏离）
    /// 用于卖出：开空/平多
    /// deviation = (bid - bid_ema) / bid_ema
    fn bid_up_deviation(&self, exchange: Exchange, bbo: &BBO) -> Option<f64> {
        let ema = self.exchange_emas.get(&exchange)?.bid_ema.value()?;
        if ema <= 0.0 {
            return None;
        }
        Some((bbo.bid_price - ema) / ema)
    }

    /// 计算某交易所 ask 的向下偏离（正值表示向下偏离）
    /// 用于买入：开多/平空
    /// deviation = (ask_ema - ask) / ask_ema
    fn ask_down_deviation(&self, exchange: Exchange, bbo: &BBO) -> Option<f64> {
        let ema = self.exchange_emas.get(&exchange)?.ask_ema.value()?;
        if ema <= 0.0 {
            return None;
        }
        Some((ema - bbo.ask_price) / ema)
    }

    /// 计算基于剩余时间的资费差（日化）
    ///
    /// 使用统一时间基准：所有交易所中最近的结算时间
    /// 如果最大时间戳 - 最小时间戳 > 2分钟，返回 None 并打印警告
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

        // 使用统一时间基准计算日化费率
        // base_settle_time: 所有交易所中最近的结算时间
        // current_time: 最新的数据时间戳
        let base_settle_time = short_rate.next_settle_time.min(long_rate.next_settle_time);
        let current_time = short_rate.timestamp.max(long_rate.timestamp);

        let short_daily = short_rate.daily_rate_with_base_time(base_settle_time, current_time);
        let long_daily = long_rate.daily_rate_with_base_time(base_settle_time, current_time);
        let spread = short_daily - long_daily;

        tracing::debug!(
            symbol = %self.symbol,
            short_exchange = %short_ex,
            short_rate = format!("{:.6}", short_rate.rate),
            short_daily = format!("{:.6}", short_daily),
            short_settle_time = short_rate.next_settle_time,
            long_exchange = %long_ex,
            long_rate = format!("{:.6}", long_rate.rate),
            long_daily = format!("{:.6}", long_daily),
            long_settle_time = long_rate.next_settle_time,
            base_settle_time = base_settle_time,
            funding_spread = format!("{:.6}", spread),
            "Funding spread calculated with unified time base"
        );

        Some((short_daily, long_daily, spread))
    }

    /// 检查开仓条件，返回开仓信号
    ///
    /// 条件：
    /// 1. 资费差满足最低要求（根据资费差动态计算阈值）
    /// 2. 资费最高交易所 bid 向上偏离 + 资费最低交易所 ask 向下偏离 ≥ open_threshold
    /// 3. 风控检查通过
    fn check_open_signal(
        &self,
        state: &SymbolState,
        state_manager: &StateManager,
    ) -> Option<OpenSignal> {
        // 检查资费差（基于剩余时间的日化）
        let funding_spread = match self.calculate_funding_spread(state) {
            Some((_, _, spread)) => spread,
            None => return None,
        };

        // 根据资费差计算动态阈值
        let (open_threshold, _) = match calculate_thresholds(funding_spread) {
            Some(thresholds) => thresholds,
            None => {
                tracing::debug!(
                    symbol = %self.symbol,
                    funding_spread = format!("{:.6}", funding_spread),
                    "Funding spread too low for opening (< 0.2%)"
                );
                return None;
            }
        };

        // 获取资费最高和最低的交易所
        let (short_exchange, _) = state.best_short_exchange()?;
        let (long_exchange, _) = state.best_long_exchange()?;

        let short_bbo = state.bbo(short_exchange)?;
        let long_bbo = state.bbo(long_exchange)?;

        // 计算偏离
        // short_exchange: bid 向上偏离（卖出开空）
        // long_exchange: ask 向下偏离（买入开多）
        let short_deviation = self.bid_up_deviation(short_exchange, short_bbo)?;
        let long_deviation = self.ask_down_deviation(long_exchange, long_bbo)?;
        let total_deviation = short_deviation + long_deviation;
        if total_deviation < open_threshold {
            return None;
        }

        // 风控检查
        let short_equity = state_manager.equity(short_exchange);
        let long_equity = state_manager.equity(long_exchange);

        if short_equity <= 0.0 || long_equity <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                short_exchange = %short_exchange,
                short_equity = short_equity,
                long_exchange = %long_exchange,
                long_equity = long_equity,
                "Insufficient equity"
            );
            return None;
        }

        // 检查单边仓位比例是否超限（包括本次开仓后的预期仓位）
        let mid_price = (short_bbo.bid_price + long_bbo.ask_price) / 2.0;
        let short_pos = state.position(short_exchange).map(|p| p.size.abs()).unwrap_or(0.0);
        let long_pos = state.position(long_exchange).map(|p| p.size.abs()).unwrap_or(0.0);

        // 预估本次开仓数量（按 max_notional 计算）
        let expected_open_qty = self.config.max_notional / mid_price;

        // 计算开仓后的预期仓位价值
        let short_pos_value_after = (short_pos + expected_open_qty) * mid_price;
        let long_pos_value_after = (long_pos + expected_open_qty) * mid_price;
        let short_pos_ratio_after = short_pos_value_after / short_equity;
        let long_pos_ratio_after = long_pos_value_after / long_equity;

        if short_pos_ratio_after >= self.config.max_position_ratio || long_pos_ratio_after >= self.config.max_position_ratio {
            return None;
        }

        tracing::info!(
            symbol = %self.symbol,
            short_exchange = %short_exchange,
            short_deviation = format!("{:.6}", short_deviation),
            short_pos_ratio = format!("{:.4}", short_pos_ratio_after),
            long_exchange = %long_exchange,
            long_deviation = format!("{:.6}", long_deviation),
            long_pos_ratio = format!("{:.4}", long_pos_ratio_after),
            total_deviation = format!("{:.6}", total_deviation),
            funding_spread = format!("{:.6}", funding_spread),
            open_threshold = format!("{:.6}", open_threshold),
            "Opening signal detected"
        );

        Some(OpenSignal {
            short_exchange,
            short_price: short_bbo.bid_price,
            long_exchange,
            long_price: long_bbo.ask_price,
        })
    }

    /// 检查平仓条件，返回平仓信号
    ///
    /// 逻辑：
    /// 1. 将持仓分为多头组和空头组
    /// 2. 多头组中找 bid 向上偏离最大的交易所（平多卖出用 bid）
    /// 3. 空头组中找 ask 向下偏离最大的交易所（平空买入用 ask）
    /// 4. 两者偏离之和 < close_threshold（负数）时触发平仓
    fn check_close_signal(&self, state: &SymbolState) -> Option<CloseSignal> {
        if !state.has_positions() {
            return None;
        }

        // 获取当前资费差，用于计算动态平仓阈值
        let funding_spread = self.calculate_funding_spread(state)
            .map(|(_, _, spread)| spread)
            .unwrap_or(0.002); // 默认使用最低档

        let (_, close_threshold) = calculate_thresholds(funding_spread)
            .unwrap_or((0.002, -0.001)); // 默认使用最低档阈值

        // 收集多头和空头交易所
        let mut long_positions: Vec<(Exchange, f64)> = Vec::new(); // (exchange, size)
        let mut short_positions: Vec<(Exchange, f64)> = Vec::new();

        for (exchange, pos) in &state.positions {
            if pos.size > 1e-10 {
                long_positions.push((*exchange, pos.size));
            } else if pos.size < -1e-10 {
                short_positions.push((*exchange, pos.size));
            }
        }

        // 需要两边都有持仓
        if long_positions.is_empty() || short_positions.is_empty() {
            return None;
        }

        // 多头组中找 bid 向上偏离最大的
        let best_long = long_positions
            .iter()
            .filter_map(|(ex, size)| {
                let bbo = state.bbo(*ex)?;
                let deviation = self.bid_up_deviation(*ex, bbo)?;
                Some((*ex, bbo.bid_price, *size, deviation))
            })
            .max_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))?;

        // 空头组中找 ask 向下偏离最大的
        let best_short = short_positions
            .iter()
            .filter_map(|(ex, size)| {
                let bbo = state.bbo(*ex)?;
                let deviation = self.ask_down_deviation(*ex, bbo)?;
                Some((*ex, bbo.ask_price, *size, deviation))
            })
            .max_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))?;

        let total_deviation = best_long.3 + best_short.3;

        // close_threshold 是负数，当 total_deviation < close_threshold 时平仓
        // 即价差收窄到反向偏离时平仓
        if total_deviation > close_threshold {
            return None;
        }

        tracing::info!(
            symbol = %self.symbol,
            long_exchange = %best_long.0,
            long_deviation = format!("{:.6}", best_long.3),
            long_size = best_long.2,
            short_exchange = %best_short.0,
            short_deviation = format!("{:.6}", best_short.3),
            short_size = best_short.2,
            total_deviation = format!("{:.6}", total_deviation),
            funding_spread = format!("{:.6}", funding_spread),
            close_threshold = format!("{:.6}", close_threshold),
            "Closing signal detected"
        );

        Some(CloseSignal {
            long_exchange: best_long.0,
            long_price: best_long.1,
            long_size: best_long.2,
            short_exchange: best_short.0,
            short_price: best_short.1,
            short_size: best_short.2,
        })
    }

    /// 检查是否需要强制 rebalance
    ///
    /// 敞口比例 = |exposure| / min(|long|, |short|)
    /// 敞口价值 = |exposure| * price
    /// 需同时超过 max_exposure_ratio 和 max_exposure_value 才触发 rebalance
    ///
    /// 返回 Some((需要平仓的交易所, 需要平的数量)) 或 None
    fn check_rebalance_needed(&self, state: &SymbolState) -> Option<(Exchange, f64)> {
        let (long_size, short_size) = state.position_sizes();

        // 无持仓或只有单边持仓，不需要 rebalance
        if long_size.abs() < 1e-10 || short_size.abs() < 1e-10 {
            return None;
        }

        let exposure = long_size + short_size; // short_size 是负数
        let min_position = long_size.abs().min(short_size.abs());
        let exposure_ratio = exposure.abs() / min_position;

        // 检查敞口比例
        if exposure_ratio <= self.config.max_exposure_ratio {
            return None;
        }

        // 获取价格计算敞口价值
        let price = state
            .bbos
            .values()
            .next()
            .map(|bbo| bbo.mid_price())
            .unwrap_or(0.0);

        let exposure_value = exposure.abs() * price;

        // 需同时超过比例和价值阈值
        if exposure_value <= self.config.max_exposure_value {
            return None;
        }

        // 需要 rebalance：平掉多的那边
        // exposure > 0: long 多了，平 long
        // exposure < 0: short 多了，平 short
        let rebalance_qty = exposure.abs();

        // 找到需要平仓的交易所
        let target_exchange = if exposure > 0.0 {
            // long 多了，找 long 的交易所
            state.positions.iter()
                .find(|(_, pos)| pos.size > 1e-10)
                .map(|(ex, _)| *ex)
        } else {
            // short 多了，找 short 的交易所
            state.positions.iter()
                .find(|(_, pos)| pos.size < -1e-10)
                .map(|(ex, _)| *ex)
        };

        target_exchange.map(|ex| {
            tracing::info!(
                symbol = %self.symbol,
                long_size = long_size,
                short_size = short_size,
                exposure = exposure,
                exposure_ratio = format!("{:.4}", exposure_ratio),
                exposure_value = format!("{:.2}", exposure_value),
                target_exchange = %ex,
                rebalance_qty = rebalance_qty,
                "Rebalance needed due to exposure exceeding limits"
            );
            (ex, rebalance_qty)
        })
    }

    /// 生成 rebalance 订单
    fn make_rebalance_order(&self, state: &SymbolState, exchange: Exchange, qty: f64) -> Option<Order> {
        let pos = state.position(exchange)?;
        let bbo = state.bbo(exchange)?;

        // 计算带滑点的价格（模拟市价单）
        let (side, price) = if pos.size > 0.0 {
            // 平多：卖出，bid - slippage
            (Side::Short, bbo.bid_price * (1.0 - MARKET_ORDER_SLIPPAGE))
        } else {
            // 平空：买入，ask + slippage
            (Side::Long, bbo.ask_price * (1.0 + MARKET_ORDER_SLIPPAGE))
        };

        if price <= 0.0 {
            return None;
        }

        // 确保不超过当前持仓
        let qty = qty.min(pos.size.abs());

        // 检查最小下单金额
        let notional = qty * price;
        if notional < self.config.min_notional {
            tracing::debug!(
                symbol = %self.symbol,
                exchange = %exchange,
                qty = qty,
                notional = notional,
                min_notional = self.config.min_notional,
                "Rebalance order below min_notional, skipping"
            );
            return None;
        }

        tracing::info!(
            symbol = %self.symbol,
            exchange = %exchange,
            side = ?side,
            price = price,
            qty = qty,
            "Generating rebalance order"
        );

        Some(Order {
            id: String::new(),
            exchange,
            symbol: self.symbol.clone(),
            side,
            order_type: OrderType::Limit {
                price,
                tif: TimeInForce::IOC,
            },
            quantity: qty,
            reduce_only: true,
            client_order_id: String::new(),
        })
    }

    /// 生成开仓订单
    ///
    /// 按固定USD金额计算开仓数量，如果当前持仓不平衡，在短缺一边增加不平衡量
    fn make_open_orders(&self, signal: &OpenSignal, state: &SymbolState) -> Vec<Order> {
        if signal.short_price <= 0.0 || signal.long_price <= 0.0 {
            return vec![];
        }

        // 按固定USD金额计算基础数量
        let base_qty = self.config.max_notional / signal.short_price.max(signal.long_price);

        if base_qty <= 0.0 {
            return vec![];
        }

        // 计算当前持仓不平衡量
        let (long_size, short_size) = state.position_sizes();
        let imbalance = long_size + short_size;

        let (short_qty, long_qty) = if imbalance.abs() < 1e-10 {
            (base_qty, base_qty)
        } else if imbalance > 0.0 {
            // 多头多了，空头需要补上不平衡量
            (base_qty + imbalance, base_qty)
        } else {
            // 空头多了，多头需要补上不平衡量
            (base_qty, base_qty + (-imbalance))
        };

        // 计算带滑点的价格（模拟市价单）
        // short_price 是 bid，做空用 bid - slippage
        // long_price 是 ask，做多用 ask + slippage
        let short_limit_price = signal.short_price * (1.0 - MARKET_ORDER_SLIPPAGE);
        let long_limit_price = signal.long_price * (1.0 + MARKET_ORDER_SLIPPAGE);

        // 检查最小下单金额（任一边低于 min_notional 则放弃本次套利）
        let short_notional = short_qty * short_limit_price;
        let long_notional = long_qty * long_limit_price;
        if short_notional < self.config.min_notional || long_notional < self.config.min_notional {
            tracing::debug!(
                symbol = %self.symbol,
                short_notional = short_notional,
                long_notional = long_notional,
                min_notional = self.config.min_notional,
                "Open orders below min_notional, skipping"
            );
            return vec![];
        }

        tracing::info!(
            symbol = %self.symbol,
            short_ex = %signal.short_exchange,
            short_price = short_limit_price,
            short_qty = short_qty,
            long_ex = %signal.long_exchange,
            long_price = long_limit_price,
            long_qty = long_qty,
            base_qty = base_qty,
            imbalance = imbalance,
            "Opening positions"
        );

        vec![
            Order {
                id: String::new(),
                exchange: signal.short_exchange,
                symbol: self.symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Limit {
                    price: short_limit_price,
                    tif: TimeInForce::IOC,
                },
                quantity: short_qty,
                reduce_only: false,
                client_order_id: String::new(),
            },
            Order {
                id: String::new(),
                exchange: signal.long_exchange,
                symbol: self.symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price: long_limit_price,
                    tif: TimeInForce::IOC,
                },
                quantity: long_qty,
                reduce_only: false,
                client_order_id: String::new(),
            },
        ]
    }

    /// 根据挂单量限制订单数量（不超过该价位挂单量的一半）
    ///
    /// - 单笔订单（rebalance）：直接 min 处理
    /// - 双笔订单（open/close）：保持多空数量一致，取两边限制后的最小值
    fn apply_orderbook_limit(&self, orders: Vec<Order>, state: &SymbolState) -> Vec<Order> {
        if orders.is_empty() {
            return orders;
        }

        // 计算单个订单的挂单量限制
        let get_limit = |order: &Order| -> Option<f64> {
            let bbo = state.bbo(order.exchange)?;
            // 卖出用 bid_qty，买入用 ask_qty
            let orderbook_qty = match order.side {
                Side::Short => bbo.bid_qty,
                Side::Long => bbo.ask_qty,
            };
            Some(orderbook_qty / 2.0)
        };

        if orders.len() == 1 {
            // rebalance 订单：直接 min 处理
            let mut order = orders.into_iter().next().unwrap();
            if let Some(limit) = get_limit(&order) {
                let original_qty = order.quantity;
                order.quantity = order.quantity.min(limit);
                if order.quantity < original_qty {
                    tracing::info!(
                        symbol = %self.symbol,
                        exchange = %order.exchange,
                        original_qty = original_qty,
                        limited_qty = order.quantity,
                        orderbook_limit = limit,
                        "Rebalance order quantity limited by orderbook"
                    );
                }
            }
            vec![order]
        } else if orders.len() == 2 {
            // open/close 订单：保持多空数量一致
            let limits: Vec<Option<f64>> = orders.iter().map(get_limit).collect();

            // 计算两边都能接受的最小数量
            let min_limit = limits
                .iter()
                .filter_map(|l| *l)
                .fold(f64::MAX, f64::min);

            let original_qty = orders[0].quantity.min(orders[1].quantity);
            let final_qty = if min_limit < f64::MAX {
                original_qty.min(min_limit)
            } else {
                original_qty
            };

            if final_qty < original_qty {
                tracing::info!(
                    symbol = %self.symbol,
                    original_qty = original_qty,
                    limited_qty = final_qty,
                    limits = ?limits,
                    "Open/Close order quantity limited by orderbook"
                );
            }

            orders
                .into_iter()
                .map(|mut order| {
                    order.quantity = final_qty;
                    order
                })
                .collect()
        } else {
            // 不应该出现其他数量，原样返回
            orders
        }
    }

    /// 生成平仓订单
    ///
    /// 基于平仓信号，以较小持仓为准生成订单
    fn make_close_orders(&self, signal: &CloseSignal) -> Vec<Order> {
        if signal.long_price <= 0.0 || signal.short_price <= 0.0 {
            tracing::info!(
                symbol = %self.symbol,
                long_price = signal.long_price,
                short_price = signal.short_price,
                "make_close_orders: invalid price, skipping"
            );
            return vec![];
        }

        // 以较小持仓为准，避免产生敞口
        // long_size 是正数，short_size 是负数
        let close_qty = signal.long_size.min(signal.short_size.abs());

        if close_qty < 1e-10 {
            tracing::info!(
                symbol = %self.symbol,
                long_size = signal.long_size,
                short_size = signal.short_size,
                close_qty = close_qty,
                "make_close_orders: close_qty too small, skipping"
            );
            return vec![];
        }

        // 计算带滑点的价格（模拟市价单）
        // long_price 是 bid，平多卖出用 bid - slippage
        // short_price 是 ask，平空买入用 ask + slippage
        let close_long_price = signal.long_price * (1.0 - MARKET_ORDER_SLIPPAGE);
        let close_short_price = signal.short_price * (1.0 + MARKET_ORDER_SLIPPAGE);

        // 检查最小下单金额（任一边低于 min_notional 则放弃本次平仓）
        let long_notional = close_qty * close_long_price;
        let short_notional = close_qty * close_short_price;
        if long_notional < self.config.min_notional || short_notional < self.config.min_notional {
            tracing::info!(
                symbol = %self.symbol,
                close_qty = close_qty,
                long_notional = long_notional,
                short_notional = short_notional,
                min_notional = self.config.min_notional,
                "make_close_orders: notional below min, skipping"
            );
            return vec![];
        }

        tracing::info!(
            symbol = %self.symbol,
            long_exchange = %signal.long_exchange,
            long_price = close_long_price,
            long_size = signal.long_size,
            short_exchange = %signal.short_exchange,
            short_price = close_short_price,
            short_size = signal.short_size,
            close_qty = close_qty,
            "Closing positions"
        );

        vec![
            // 平多：卖出
            Order {
                id: String::new(),
                exchange: signal.long_exchange,
                symbol: self.symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Limit {
                    price: close_long_price,
                    tif: TimeInForce::IOC,
                },
                quantity: close_qty,
                reduce_only: true,
                client_order_id: String::new(),
            },
            // 平空：买入
            Order {
                id: String::new(),
                exchange: signal.short_exchange,
                symbol: self.symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price: close_short_price,
                    tif: TimeInForce::IOC,
                },
                quantity: close_qty,
                reduce_only: true,
                client_order_id: String::new(),
            },
        ]
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

        // BBO 事件时更新对应交易所的 EMA
        if let ExchangeEventData::BBO(bbo) = &event.data {
            self.update_ema(bbo.exchange, bbo);
        }

        // EMA 未预热完成，不进行交易
        if !self.all_emas_ready() {
            return vec![];
        }

        // 有未完成订单时等待
        if symbol_state.has_pending_orders() {
            return vec![];
        }

        // 步骤 2: 敞口超限 → rebalance（平掉多余仓位）
        if let Some((exchange, qty)) = self.check_rebalance_needed(symbol_state) {
            if let Some(order) = self.make_rebalance_order(symbol_state, exchange, qty) {
                let orders = self.apply_orderbook_limit(vec![order], symbol_state);
                return orders.into_iter().map(OutcomeEvent::PlaceOrder).collect();
            }
            return vec![];
        }

        // 步骤 3: 检查平仓条件
        if let Some(close_signal) = self.check_close_signal(symbol_state) {
            let orders = self.make_close_orders(&close_signal);
            tracing::info!(
                symbol = %self.symbol,
                orders_count = orders.len(),
                "Close orders generated from make_close_orders"
            );
            let orders = self.apply_orderbook_limit(orders, symbol_state);
            tracing::info!(
                symbol = %self.symbol,
                orders_count = orders.len(),
                "Close orders after apply_orderbook_limit"
            );
            return orders.into_iter().map(OutcomeEvent::PlaceOrder).collect();
        }

        // 步骤 4: 检查开仓条件
        if let Some(open_signal) = self.check_open_signal(symbol_state, state) {
            let orders = self.make_open_orders(&open_signal, symbol_state);
            let orders = self.apply_orderbook_limit(orders, symbol_state);
            return orders.into_iter().map(OutcomeEvent::PlaceOrder).collect();
        }

        vec![]
    }
}

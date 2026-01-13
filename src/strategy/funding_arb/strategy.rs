use crate::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager, SymbolState};
use crate::strategy::{OutcomeEvent, Strategy};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::config::FundingArbConfig;
use super::ema::ExchangeEma;
use super::signals::TradingSignal;

/// 仓位比较的 epsilon（用于判断仓位是否为零）
const POSITION_EPSILON: f64 = 1e-10;

/// 跨所价差套利策略 (单 symbol)
///
/// 策略逻辑：
/// 1. 为每个交易所的 bid 和 ask 分别维护 EMA（表示最近 N 笔 BBO 更新的均价）
/// 2. 当 BBO 更新时，更新对应交易所的 bid_ema 和 ask_ema
/// 3. 计算每个交易所的偏离度：
///    - bid_deviation = bid / bid_ema - 1（正值表示当前 bid 高于均值，适合卖出）
///    - ask_deviation = ask_ema / ask - 1（正值表示当前 ask 低于均值，适合买入）
/// 4. 找到 max_bid_deviation 最大的交易所（卖出）和 max_ask_deviation 最大的交易所（买入）
/// 5. 如果 max_bid_deviation + max_ask_deviation > threshold，则下单
/// 6. 下单前经过 pipeline 处理：合法性检查 → 杠杆率检查 → 净敞口修正 → notional 检查
pub struct FundingArbStrategy {
    config: FundingArbConfig,
    exchanges: Vec<Exchange>,
    symbol: Symbol,
    /// 每个交易所的 bid/ask EMA
    exchange_emas: HashMap<Exchange, ExchangeEma>,
}

impl FundingArbStrategy {
    pub fn new(config: FundingArbConfig, exchanges: Vec<Exchange>, symbol: Symbol) -> Self {
        // 为每个交易所创建 bid/ask EMA
        let mut exchange_emas = HashMap::new();
        for &ex in &exchanges {
            exchange_emas.insert(ex, ExchangeEma::new(config.ema_period));
        }

        Self {
            config,
            exchanges,
            symbol,
            exchange_emas,
        }
    }

    /// 更新指定交易所的 bid/ask EMA
    fn update_exchange_ema(&mut self, exchange: Exchange, state: &SymbolState) {
        if let Some(bbo) = state.bbo(exchange) {
            let ema = self.exchange_emas.get_mut(&exchange)
                .expect("exchange must exist in exchange_emas");
            ema.bid_ema.update(bbo.bid_price);
            ema.ask_ema.update(bbo.ask_price);
        }
    }

    /// 检查所有交易所的 EMA 是否都预热完成
    fn all_emas_ready(&self) -> bool {
        self.exchange_emas.values().all(|ema| ema.is_ready())
    }

    /// 计算单个交易所的 bid deviation
    /// bid_deviation = bid / bid_ema - 1
    /// 正值表示当前 bid 高于均值，适合卖出
    /// 注意：EMA 必须预热完成（满 ema_period 条）才返回有效值
    fn bid_deviation(&self, exchange: Exchange, state: &SymbolState) -> Option<f64> {
        let bbo = state.bbo(exchange)?;
        let ema = self.exchange_emas.get(&exchange)?;

        // EMA 必须预热完成才参与比较
        if !ema.bid_ema.is_ready() {
            return None;
        }

        let bid_ema = ema.bid_ema.value()?;

        if bid_ema <= 0.0 {
            return None;
        }

        Some(bbo.bid_price / bid_ema - 1.0)
    }

    /// 计算单个交易所的 ask deviation
    /// ask_deviation = ask_ema / ask - 1
    /// 正值表示当前 ask 低于均值，适合买入
    /// 注意：EMA 必须预热完成（满 ema_period 条）才返回有效值
    fn ask_deviation(&self, exchange: Exchange, state: &SymbolState) -> Option<f64> {
        let bbo = state.bbo(exchange)?;
        let ema = self.exchange_emas.get(&exchange)?;

        // EMA 必须预热完成才参与比较
        if !ema.ask_ema.is_ready() {
            return None;
        }

        let ask_ema = ema.ask_ema.value()?;

        if bbo.ask_price <= 0.0 {
            return None;
        }

        Some(ask_ema / bbo.ask_price - 1.0)
    }

    // ========== 信号检测 ==========

    /// 检查交易信号
    ///
    /// 逻辑：
    /// 1. 找到 bid_deviation 最大的交易所（卖出）
    /// 2. 找到 ask_deviation 最大的交易所（买入）
    /// 3. 根据 symbol 杠杆率计算动态阈值（杠杆率越高，阈值越低）
    /// 4. 如果 max_bid_deviation + max_ask_deviation > effective_threshold，且两个交易所不同，则生成信号
    /// 5. 信号包含盘口的 size（bid_qty / ask_qty）
    fn check_signal(&self, state: &SymbolState, state_manager: &StateManager) -> Option<TradingSignal> {
        // 找到 bid_deviation 最大的交易所（卖出）
        let max_bid_dev = self.exchanges.iter()
            .filter_map(|&ex| self.bid_deviation(ex, state).map(|dev| (ex, dev)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));

        // 找到 ask_deviation 最大的交易所（买入）
        let max_ask_dev = self.exchanges.iter()
            .filter_map(|&ex| self.ask_deviation(ex, state).map(|dev| (ex, dev)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));

        let (short_exchange, bid_deviation) = max_bid_dev?;
        let (long_exchange, ask_deviation) = max_ask_dev?;

        // 两个交易所必须不同
        if short_exchange == long_exchange {
            return None;
        }

        // 计算动态阈值：基于参与交易的两个交易所的 symbol 杠杆率调整
        // 杠杆率越高，阈值越低（更容易开仓以降低仓位）
        let effective_threshold = self.calculate_effective_threshold(
            short_exchange,
            long_exchange,
            state,
            state_manager,
        );

        // 检查 deviation 之和是否超过动态阈值
        let total_deviation = bid_deviation + ask_deviation;
        if total_deviation < effective_threshold {
            return None;
        }

        let long_bbo = state.bbo(long_exchange)?;
        let short_bbo = state.bbo(short_exchange)?;

        // 使用盘口较小一侧的一半作为基础开仓数量
        let base_size = (long_bbo.ask_qty / 2.0).min(short_bbo.bid_qty / 2.0);

        Some(TradingSignal {
            long_exchange,
            long_price: long_bbo.ask_price,
            long_size: base_size,
            long_book_qty: long_bbo.ask_qty,
            short_exchange,
            short_price: short_bbo.bid_price,
            short_size: base_size,
            short_book_qty: short_bbo.bid_qty,
            long_deviation: ask_deviation,
            short_deviation: bid_deviation,
        })
    }

    /// 计算有效开仓阈值
    ///
    /// 根据杠杆率高的一边的仓位方向动态调整阈值：
    /// - 开仓/加仓方向 → 阈值提高（更难触发，控制风险）
    /// - 平仓方向 → 阈值降低（更易触发，降低风险）
    ///
    /// 公式: effective_threshold = base * (1 + direction_factor * leverage_ratio)
    /// - direction_factor = sign(position * order_direction)
    /// - 同向/新开仓 = +1，反向 = -1
    fn calculate_effective_threshold(
        &self,
        short_exchange: Exchange,
        long_exchange: Exchange,
        state: &SymbolState,
        state_manager: &StateManager,
    ) -> f64 {
        let base_threshold = self.config.deviation_threshold;
        let max_symbol_leverage = self.config.max_symbol_leverage;

        // 计算单个交易所的 symbol 杠杆率
        let calc_leverage = |exchange: Exchange| -> f64 {
            let equity = state_manager.equity(exchange);
            if equity <= 0.0 {
                return 0.0;
            }
            let pos_size = state.position(exchange).map(|p| p.size.abs()).unwrap_or(0.0);
            let price = state.bbo(exchange).map(|b| b.mid_price()).unwrap_or(0.0);
            (pos_size * price) / equity
        };

        let short_leverage = calc_leverage(short_exchange);
        let long_leverage = calc_leverage(long_exchange);

        // 获取仓位
        let short_pos = state.position(short_exchange).map(|p| p.size).unwrap_or(0.0);
        let long_pos = state.position(long_exchange).map(|p| p.size).unwrap_or(0.0);

        // 计算方向因子：position * order_direction 的符号
        // long 做多 (+1)，short 做空 (-1)
        // 同向/新开仓 → +1，反向 → -1
        let long_factor = (long_pos + f64::EPSILON).signum();
        let short_factor = (short_pos * -1.0 + f64::EPSILON).signum();

        // 以杠杆率高的一边为参考
        let (leverage_ratio, direction_factor) = if short_leverage >= long_leverage {
            (short_leverage / max_symbol_leverage, short_factor)
        } else {
            (long_leverage / max_symbol_leverage, long_factor)
        };

        // 统一公式：开仓时阈值提高，平仓时阈值降低
        let effective_threshold = base_threshold * (1.0 + direction_factor * leverage_ratio);

        tracing::debug!(
            symbol = %self.symbol,
            base_threshold = format!("{:.4}", base_threshold),
            short_exchange = %short_exchange,
            short_leverage = format!("{:.4}", short_leverage),
            short_pos = format!("{:.4}", short_pos),
            short_factor = format!("{:.1}", short_factor),
            long_exchange = %long_exchange,
            long_leverage = format!("{:.4}", long_leverage),
            long_pos = format!("{:.4}", long_pos),
            long_factor = format!("{:.1}", long_factor),
            direction_factor = format!("{:.1}", direction_factor),
            leverage_ratio = format!("{:.4}", leverage_ratio),
            effective_threshold = format!("{:.4}", effective_threshold),
            "Calculated effective threshold"
        );

        effective_threshold
    }

    // ========== Pipeline 处理 ==========

    /// Pipeline 第1步：合法性检查
    ///
    /// 检查各字段是否有效，无效的设置 size 为 0
    fn validate_signal(&self, signal: &mut TradingSignal) {
        if signal.long_price <= 0.0 || signal.short_price <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                long_price = signal.long_price,
                short_price = signal.short_price,
                "Signal filtered: invalid price"
            );
            signal.long_size = 0.0;
            signal.short_size = 0.0;
            return;
        }

        if signal.long_size < 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                long_size = signal.long_size,
                "Signal filtered: negative long_size"
            );
            signal.long_size = 0.0;
        }
        if signal.short_size < 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                short_size = signal.short_size,
                "Signal filtered: negative short_size"
            );
            signal.short_size = 0.0;
        }
    }

    /// Pipeline 第3步：杠杆率检查
    ///
    /// 基于调整后的 signal.long_size/short_size 计算新杠杆率
    /// 如果 new_leverage > old_leverage 且 new_leverage 超过阈值，则将对应侧 size 设为 0
    fn check_symbol_leverage(
        &self,
        signal: &mut TradingSignal,
        state: &SymbolState,
        state_manager: &StateManager,
    ) {
        let short_equity = state_manager.equity(signal.short_exchange);
        let long_equity = state_manager.equity(signal.long_exchange);

        if short_equity <= 0.0 || long_equity <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                short_exchange = %signal.short_exchange,
                short_equity = short_equity,
                long_exchange = %signal.long_exchange,
                long_equity = long_equity,
                "Insufficient equity"
            );
            signal.long_size = 0.0;
            signal.short_size = 0.0;
            return;
        }

        let mid_price = (signal.short_price + signal.long_price) / 2.0;

        // 当前持仓
        let short_pos = state.position(signal.short_exchange).map(|p| p.size.abs()).unwrap_or(0.0);
        let long_pos = state.position(signal.long_exchange).map(|p| p.size.abs()).unwrap_or(0.0);

        // 当前杠杆率
        let old_short_leverage = (short_pos * mid_price) / short_equity;
        let old_long_leverage = (long_pos * mid_price) / long_equity;

        // 新杠杆率（基于 signal 中已调整后的 size）
        let new_short_leverage = ((short_pos + signal.short_size) * mid_price) / short_equity;
        let new_long_leverage = ((long_pos + signal.long_size) * mid_price) / long_equity;

        // 检查：如果新杠杆率 > 旧杠杆率 且 新杠杆率超过阈值，则将对应侧 size 设为 0
        let short_blocked = new_short_leverage > old_short_leverage
            && new_short_leverage >= self.config.max_symbol_leverage;
        let long_blocked = new_long_leverage > old_long_leverage
            && new_long_leverage >= self.config.max_symbol_leverage;

        if short_blocked {
            signal.short_size = 0.0;
        }
        if long_blocked {
            signal.long_size = 0.0;
        }

        if short_blocked || long_blocked {
            tracing::info!(
                symbol = %self.symbol,
                old_short_leverage = format!("{:.4}", old_short_leverage),
                new_short_leverage = format!("{:.4}", new_short_leverage),
                old_long_leverage = format!("{:.4}", old_long_leverage),
                new_long_leverage = format!("{:.4}", new_long_leverage),
                max_symbol_leverage = format!("{:.4}", self.config.max_symbol_leverage),
                short_blocked = short_blocked,
                long_blocked = long_blocked,
                "Signal adjusted: leverage exceeds threshold"
            );
        }
    }

    /// Pipeline 第4步：账户杠杆率检查
    ///
    /// 检查账户级别杠杆率 (account_notional / equity)
    /// 如果某交易所杠杆率超过阈值，且订单方向与现有仓位方向相同，则将对应侧 size 设为 0
    fn check_account_leverage(
        &self,
        signal: &mut TradingSignal,
        state: &SymbolState,
        state_manager: &StateManager,
    ) {
        // 计算两边交易所的账户杠杆率
        let short_equity = state_manager.equity(signal.short_exchange);
        let short_notional = state_manager.account_notional(signal.short_exchange);
        let short_leverage = if short_equity > 0.0 {
            short_notional / short_equity
        } else {
            0.0
        };

        let long_equity = state_manager.equity(signal.long_exchange);
        let long_notional = state_manager.account_notional(signal.long_exchange);
        let long_leverage = if long_equity > 0.0 {
            long_notional / long_equity
        } else {
            0.0
        };

        // 获取当前 symbol 在各交易所的仓位
        let short_pos = state
            .position(signal.short_exchange)
            .map(|p| p.size)
            .unwrap_or(0.0);
        let long_pos = state
            .position(signal.long_exchange)
            .map(|p| p.size)
            .unwrap_or(0.0);

        // 检查做空方：杠杆率超标 && 已有空头仓位（方向相同，会增加杠杆）
        let short_blocked =
            short_leverage >= self.config.max_account_leverage && short_pos < -POSITION_EPSILON;

        // 检查做多方：杠杆率超标 && 已有多头仓位（方向相同，会增加杠杆）
        let long_blocked =
            long_leverage >= self.config.max_account_leverage && long_pos > POSITION_EPSILON;

        if short_blocked {
            signal.short_size = 0.0;
        }
        if long_blocked {
            signal.long_size = 0.0;
        }

        if short_blocked || long_blocked {
            tracing::info!(
                symbol = %self.symbol,
                short_exchange = %signal.short_exchange,
                short_leverage = format!("{:.2}", short_leverage),
                short_pos = format!("{:.4}", short_pos),
                short_blocked = short_blocked,
                long_exchange = %signal.long_exchange,
                long_leverage = format!("{:.2}", long_leverage),
                long_pos = format!("{:.4}", long_pos),
                long_blocked = long_blocked,
                max_account_leverage = format!("{:.2}", self.config.max_account_leverage),
                "Signal adjusted: account leverage exceeds threshold"
            );
        }
    }

    /// Pipeline 第2步：净敞口修正
    ///
    /// 根据当前净敞口调整下单数量
    /// 例如：净敞口为 +10（多头多），则多头下单量减去 10（取 max(0)）
    fn adjust_for_exposure(&self, signal: &mut TradingSignal, state: &SymbolState) {
        let (long_size, short_size) = state.position_sizes();
        // net_exposure = long_size + short_size（short_size 是负数）
        // > 0 表示多头多了，< 0 表示空头多了
        let net_exposure = long_size + short_size;

        // 使用 signal 中的 size（已由 check_signal 设置为盘口限制）和 max_notional 的较小值
        let mid_price = (signal.short_price + signal.long_price) / 2.0;
        let max_qty = self.config.max_notional / mid_price;
        let base_qty = signal.long_size.min(signal.short_size).min(max_qty);

        if net_exposure.abs() < POSITION_EPSILON {
            // 无敞口，两边数量相等
            signal.long_size = base_qty;
            signal.short_size = base_qty;
        } else if net_exposure > 0.0 {
            // 多头多了，减少多头下单量，空头正常开
            signal.long_size = (base_qty - net_exposure).max(0.0);
            signal.short_size = base_qty;
        } else {
            // 空头多了，减少空头下单量，多头正常开
            let abs_exposure = net_exposure.abs();
            signal.long_size = base_qty;
            signal.short_size = (base_qty - abs_exposure).max(0.0);
        }
    }

    /// Pipeline 第4步：notional 限制
    ///
    /// - 小于 min_notional 的 size 设为 0（该侧不下单）
    /// - 大于 max_notional 的 size 限制到 max_notional
    fn set_notional_limits(&self, signal: &mut TradingSignal) {
        let min_qty_long = self.config.min_notional / signal.long_price;
        let max_qty_long = self.config.max_notional / signal.long_price;
        let min_qty_short = self.config.min_notional / signal.short_price;
        let max_qty_short = self.config.max_notional / signal.short_price;

        // 小于 min_notional 设为 0，大于 max_notional 限制到 max_notional
        if signal.long_size < min_qty_long {
            signal.long_size = 0.0;
        } else if signal.long_size > max_qty_long {
            signal.long_size = max_qty_long;
        }

        if signal.short_size < min_qty_short {
            signal.short_size = 0.0;
        } else if signal.short_size > max_qty_short {
            signal.short_size = max_qty_short;
        }
    }

    /// 运行完整的信号处理 pipeline
    ///
    /// 顺序：validate → check_account_leverage → adjust_for_exposure → set_notional_limits → check_symbol_leverage
    /// 各步骤通过将 size 设为 0 表示不开仓
    fn process_signal(
        &self,
        mut signal: TradingSignal,
        state: &SymbolState,
        state_manager: &StateManager,
    ) -> TradingSignal {
        self.validate_signal(&mut signal);
        self.check_account_leverage(&mut signal, state, state_manager);
        self.adjust_for_exposure(&mut signal, state);
        self.set_notional_limits(&mut signal);
        self.check_symbol_leverage(&mut signal, state, state_manager);
        signal
    }

    // ========== 辅助功能 ==========

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
        if long_size.abs() < POSITION_EPSILON || short_size.abs() < POSITION_EPSILON {
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
                .find(|(_, pos)| pos.size > POSITION_EPSILON)
                .map(|(ex, _)| *ex)
        } else {
            // short 多了，找 short 的交易所
            state.positions.iter()
                .find(|(_, pos)| pos.size < -POSITION_EPSILON)
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

    /// 生成 rebalance 订单，返回订单和描述
    fn make_rebalance_order(
        &self,
        state: &SymbolState,
        exchange: Exchange,
        qty: f64,
    ) -> Option<(Order, String)> {
        let pos = state.position(exchange)?;
        let bbo = state.bbo(exchange)?;

        // 计算带滑点的价格（模拟市价单）
        let (side, price, orderbook_price, orderbook_qty) = if pos.size > 0.0 {
            // 平多：卖出，bid - slippage
            (
                Side::Short,
                bbo.bid_price * (1.0 - self.config.ioc_slippage),
                bbo.bid_price,
                bbo.bid_qty,
            )
        } else {
            // 平空：买入，ask + slippage
            (
                Side::Long,
                bbo.ask_price * (1.0 + self.config.ioc_slippage),
                bbo.ask_price,
                bbo.ask_qty,
            )
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

        // 限制在盘口的一半
        let orderbook_limit = orderbook_qty / 2.0;
        let qty = qty.min(orderbook_limit);

        // 获取敞口信息
        let (long_size, short_size) = state.position_sizes();
        let exposure = long_size + short_size;

        tracing::info!(
            symbol = %self.symbol,
            exchange = %exchange,
            side = ?side,
            price = price,
            qty = qty,
            "Generating rebalance order"
        );

        let order = Order {
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
        };

        // comment: rebalance 标记 | 敞口 | 下单数量 | 对手价x挂单量
        let comment = format!(
            "rebal | exp={:.4} | qty={:.4} | book={:.4}x{:.4}",
            exposure,
            qty,
            orderbook_price,
            orderbook_qty,
        );

        Some((order, comment))
    }

    /// 根据处理后的信号生成订单，返回订单和描述的列表
    fn make_orders(&self, signal: &TradingSignal, state: &SymbolState) -> Vec<(Order, String)> {
        // 计算带滑点的价格（模拟市价单）
        // short_price 是 bid，做空用 bid - slippage
        // long_price 是 ask，做多用 ask + slippage
        let short_limit_price = signal.short_price * (1.0 - self.config.ioc_slippage);
        let long_limit_price = signal.long_price * (1.0 + self.config.ioc_slippage);

        // 获取当前敞口
        let (long_size, short_size) = state.position_sizes();
        let net_exposure = long_size + short_size;

        let mut orders = Vec::new();

        // 只有 size > 0 时才生成订单
        if signal.short_size > POSITION_EPSILON {
            let order = Order {
                id: String::new(),
                exchange: signal.short_exchange,
                symbol: self.symbol.clone(),
                side: Side::Short,
                order_type: OrderType::Limit {
                    price: short_limit_price,
                    tif: TimeInForce::IOC,
                },
                quantity: signal.short_size,
                reduce_only: false,
                client_order_id: String::new(),
            };
            // comment: 敞口 | 下单数量 | 对手价x挂单量 | deviation
            let comment = format!(
                "exp={:.4} | qty={:.4} | book={:.4}x{:.4} | dev={:.4}%",
                net_exposure,
                signal.short_size,
                signal.short_price,
                signal.short_book_qty,
                signal.short_deviation * 100.0,
            );
            orders.push((order, comment));
        }

        if signal.long_size > POSITION_EPSILON {
            let order = Order {
                id: String::new(),
                exchange: signal.long_exchange,
                symbol: self.symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price: long_limit_price,
                    tif: TimeInForce::IOC,
                },
                quantity: signal.long_size,
                reduce_only: false,
                client_order_id: String::new(),
            };
            // comment: 敞口 | 下单数量 | 对手价x挂单量 | deviation
            let comment = format!(
                "exp={:.4} | qty={:.4} | book={:.4}x{:.4} | dev={:.4}%",
                net_exposure,
                signal.long_size,
                signal.long_price,
                signal.long_book_qty,
                signal.long_deviation * 100.0,
            );
            orders.push((order, comment));
        }

        if !orders.is_empty() {
            tracing::info!(
                symbol = %self.symbol,
                short_ex = %signal.short_exchange,
                short_bid = signal.short_price,
                short_limit = short_limit_price,
                short_qty = signal.short_size,
                long_ex = %signal.long_exchange,
                long_ask = signal.long_price,
                long_limit = long_limit_price,
                long_qty = signal.long_size,
                "Placing orders"
            );
        }

        orders
    }
}

impl Strategy for FundingArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        // 纯价差套利只需要 BBO 数据，不需要资费数据
        let kinds: HashSet<SubscriptionKind> = [SubscriptionKind::BBO {
            symbol: self.symbol.clone(),
        }]
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

        // BBO 事件时更新该交易所的 bid/ask EMA
        if let ExchangeEventData::BBO(bbo) = &event.data {
            self.update_exchange_ema(bbo.exchange, symbol_state);
        }

        // EMA 未预热完成，不进行交易
        if !self.all_emas_ready() {
            return vec![];
        }

        // 有未完成订单时等待
        if symbol_state.has_pending_orders() {
            return vec![];
        }

        // 步骤 1: 敞口超限 → rebalance（平掉多余仓位）
        if let Some((exchange, qty)) = self.check_rebalance_needed(symbol_state) {
            if let Some((order, comment)) = self.make_rebalance_order(symbol_state, exchange, qty) {
                return vec![OutcomeEvent::PlaceOrder { order, comment }];
            }
            return vec![];
        }

        // 步骤 2: 检查信号并通过 pipeline 处理
        if let Some(signal) = self.check_signal(symbol_state, state) {
            let processed_signal = self.process_signal(signal, symbol_state, state);
            let orders = self.make_orders(&processed_signal, symbol_state);
            if !orders.is_empty() {
                return orders
                    .into_iter()
                    .map(|(order, comment)| OutcomeEvent::PlaceOrder { order, comment })
                    .collect();
            }
        }

        vec![]
    }
}

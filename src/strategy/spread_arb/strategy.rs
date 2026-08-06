use crate::domain::{Exchange, Order, OrderType, Position, Side, Symbol, TimeInForce, Timestamp};
use crate::exchange::SubscriptionKind;
use crate::messaging::{ExchangeEventData, IncomeEvent, StateManager, SymbolState};
use crate::strategy::{OutcomeEvent, Strategy};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::config::SpreadArbConfig;
use super::ema::ExchangeEma;
use super::signals::TradingSignal;

/// 仓位比较的 epsilon（与 domain 层同一口径，不另立副本）
const POSITION_EPSILON: f64 = Position::EPSILON;

/// 单笔开仓最多吃掉盘口首档的比例
const ORDERBOOK_TAKE_RATIO: f64 = 0.5;

/// 跨所价差至少需要两个交易所才成立
///
/// 这是策略的固有前提，放在策略模块由上层选币时引用（而不是各调用方各写一份）。
pub const MIN_EXCHANGES_PER_SYMBOL: usize = 2;

/// 跨所价差套利策略 (单 symbol)
///
/// 策略逻辑：
/// 1. 为每个交易所的 bid 和 ask 分别维护 EMA（表示最近 N 笔 BBO 更新的均价）
/// 2. 当 BBO 更新时，更新对应交易所的 bid_ema 和 ask_ema
/// 3. 计算每个交易所的偏离度：
///    - bid_deviation = bid / bid_ema - 1（正值表示当前 bid 高于均值，适合卖出）
///    - ask_deviation = ask_ema / ask - 1（正值表示当前 ask 低于均值，适合买入）
/// 4. 在**行情新鲜**的交易所中，找到 max_bid_deviation 的所（卖出）与 max_ask_deviation 的所（买入）
/// 5. 如果 max_bid_deviation + max_ask_deviation >= 动态阈值，则下单
/// 6. 下单前经过 pipeline 处理：合法性 → 账户杠杆 → 净敞口修正 → notional 限制
///    → 单边杠杆 → 单 pair 仓位上限
///
/// 成本模型：策略不单独计费，全部交易成本由 `deviation_threshold` 承担
/// （见 [`SpreadArbConfig`] 的字段说明）。
///
/// 敞口管理：两条腿是独立的 IOC 单，可能只有单腿成交。净敞口由
/// [`SpreadArbStrategy::adjust_for_exposure`] 在后续信号中逐步中和，本策略不做强制 rebalance。
pub struct SpreadArbStrategy {
    config: SpreadArbConfig,
    /// 本 symbol 实际上市的交易所（由上层按各所 symbol 列表推导，至少两个）
    exchanges: Vec<Exchange>,
    symbol: Symbol,
    /// 每个交易所的 bid/ask EMA
    exchange_emas: HashMap<Exchange, ExchangeEma>,
    /// 每个交易所最近一次 BBO 的**本地接收时刻**
    ///
    /// 用本地时刻而非交易所时间戳做新鲜度判定：跨所时钟偏移不可控，本地接收时刻是
    /// 唯一可横向比较的基准。
    bbo_local_ts: HashMap<Exchange, Timestamp>,
    /// 当前被判定为行情陈旧的交易所（仅用于状态变迁日志，不参与决策）
    stale_exchanges: HashSet<Exchange>,
}

impl SpreadArbStrategy {
    pub fn new(config: SpreadArbConfig, exchanges: Vec<Exchange>, symbol: Symbol) -> Self {
        // 少于两个交易所时永远配不出 (short, long) 组合，该实例只会空转
        if exchanges.len() < MIN_EXCHANGES_PER_SYMBOL {
            tracing::warn!(
                %symbol,
                exchanges = exchanges.len(),
                "交易所少于 2 个，该 symbol 无法产生跨所价差信号"
            );
        }

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
            bbo_local_ts: HashMap::new(),
            stale_exchanges: HashSet::new(),
        }
    }

    /// 更新指定交易所的 bid/ask EMA
    fn update_exchange_ema(&mut self, exchange: Exchange, state: &SymbolState) {
        if let Some(bbo) = state.bbo(exchange) {
            // 构造时已为每个订阅交易所建 EMA；若收到非订阅交易所的 BBO 则记录后跳过，不 panic。
            let Some(ema) = self.exchange_emas.get_mut(&exchange) else {
                tracing::warn!(exchange = %exchange, symbol = %self.symbol, "收到未订阅交易所的 BBO，跳过 EMA 更新");
                return;
            };
            ema.bid_ema.update(bbo.bid_price);
            ema.ask_ema.update(bbo.ask_price);
        }
    }

    /// 指定交易所的 BBO 是否新鲜
    ///
    /// 从未收到过 BBO 也视为不新鲜（安全侧）。
    fn is_bbo_fresh(&self, exchange: Exchange, now: Timestamp) -> bool {
        match self.bbo_local_ts.get(&exchange) {
            Some(&ts) => now.saturating_sub(ts) <= self.config.max_bbo_age_ms,
            None => false,
        }
    }

    /// 记录行情新鲜度的**状态变迁**（陈旧 ⇄ 恢复），避免每个 tick 刷日志
    ///
    /// 由 Clock 事件驱动即可生效：行情完全停推时仍有 Clock 推进时间基准。
    fn log_staleness_transitions(&mut self, now: Timestamp) {
        for &exchange in &self.exchanges {
            let fresh = match self.bbo_local_ts.get(&exchange) {
                Some(&ts) => now.saturating_sub(ts) <= self.config.max_bbo_age_ms,
                // 从未收到过 BBO 属于启动预热，不参与变迁判定
                None => continue,
            };
            let was_stale = self.stale_exchanges.contains(&exchange);
            if !fresh && !was_stale {
                let age_ms = self
                    .bbo_local_ts
                    .get(&exchange)
                    .map(|&ts| now.saturating_sub(ts))
                    .unwrap_or_default();
                tracing::warn!(
                    symbol = %self.symbol,
                    exchange = %exchange,
                    age_ms = age_ms,
                    max_bbo_age_ms = self.config.max_bbo_age_ms,
                    "行情陈旧，该交易所暂不参与信号"
                );
                self.stale_exchanges.insert(exchange);
            } else if fresh && was_stale {
                tracing::info!(
                    symbol = %self.symbol,
                    exchange = %exchange,
                    "行情恢复新鲜"
                );
                self.stale_exchanges.remove(&exchange);
            }
        }
    }

    /// 行情中断后恢复：重置该交易所的 EMA，强制重新预热
    ///
    /// **必须重置**。EMA 是无窗口指数平滑（alpha = 2/(N+1)，N=100 时约 0.02），中断期间不更新，
    /// 恢复的第一跳只能把它拉动 2%。若中断期市场真实移动了 x%，恢复瞬间算出的偏离度约等于 x%，
    /// 远超阈值——策略会把"自己错过的行情"当成错价，在**已经反映新价格**的盘口上吃单，
    /// 这是确定性亏损。新鲜度闸门只挡住了中断期间，恢复瞬间必须靠重置 EMA 挡住。
    ///
    /// 放在 BBO 到达路径上判定（而非依赖 Clock 驱动的状态变迁），保证恢复的第一跳一定被拦。
    fn reset_ema_if_gap_exceeded(&mut self, exchange: Exchange, now: Timestamp) {
        let Some(&prev_ts) = self.bbo_local_ts.get(&exchange) else {
            // 首次收到该所行情，正常预热流程
            return;
        };
        let gap_ms = now.saturating_sub(prev_ts);
        if gap_ms <= self.config.max_bbo_age_ms {
            return;
        }
        tracing::warn!(
            symbol = %self.symbol,
            exchange = %exchange,
            gap_ms = gap_ms,
            max_bbo_age_ms = self.config.max_bbo_age_ms,
            "行情中断后恢复，重置 EMA 重新预热（避免用中断前的均值算出假偏离）"
        );
        self.exchange_emas
            .insert(exchange, ExchangeEma::new(self.config.ema_period));
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
    /// 1. 在行情新鲜的交易所中，找到 bid_deviation 最大的（卖出）与 ask_deviation 最大的（买入）
    /// 2. 根据杠杆率与仓位方向计算动态阈值
    /// 3. 如果两个 deviation 之和 >= 动态阈值，且两个交易所不同，则生成信号
    /// 4. 信号包含盘口的 size（bid_qty / ask_qty）
    ///
    /// 新鲜度过滤放在**候选筛选**阶段而非信号生成之后：陈旧行情的偏离度会冻结在某个极值上，
    /// 若让它参与 max 竞争再整体否决信号，会连带屏蔽本可成交的其他交易所组合。
    fn check_signal(
        &self,
        state: &SymbolState,
        state_manager: &StateManager,
        now: Timestamp,
    ) -> Option<TradingSignal> {
        // 找到 bid_deviation 最大的交易所（卖出）
        let max_bid_dev = self
            .exchanges
            .iter()
            .filter(|&&ex| self.is_bbo_fresh(ex, now))
            .filter_map(|&ex| self.bid_deviation(ex, state).map(|dev| (ex, dev)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));

        // 找到 ask_deviation 最大的交易所（买入）
        let max_ask_dev = self
            .exchanges
            .iter()
            .filter(|&&ex| self.is_bbo_fresh(ex, now))
            .filter_map(|&ex| self.ask_deviation(ex, state).map(|dev| (ex, dev)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));

        let (short_exchange, bid_deviation) = max_bid_dev?;
        let (long_exchange, ask_deviation) = max_ask_dev?;

        // 两个交易所必须不同
        if short_exchange == long_exchange {
            return None;
        }

        // 计算动态阈值：基于参与交易的两个交易所的 symbol 杠杆率调整
        let effective_threshold =
            self.calculate_effective_threshold(short_exchange, long_exchange, state, state_manager);

        // 检查 deviation 之和是否超过动态阈值
        let total_deviation = bid_deviation + ask_deviation;
        if total_deviation < effective_threshold {
            return None;
        }

        let long_bbo = state.bbo(long_exchange)?;
        let short_bbo = state.bbo(short_exchange)?;

        // 使用盘口较小一侧的一部分作为基础开仓数量
        let base_size = (long_bbo.ask_qty * ORDERBOOK_TAKE_RATIO)
            .min(short_bbo.bid_qty * ORDERBOOK_TAKE_RATIO);

        Some(TradingSignal {
            long_exchange,
            long_price: long_bbo.ask_price,
            long_size: base_size,
            short_exchange,
            short_price: short_bbo.bid_price,
            short_size: base_size,
            long_deviation: ask_deviation,
            short_deviation: bid_deviation,
        })
    }

    /// 计算有效开仓阈值
    ///
    /// 根据杠杆率高的一边的仓位方向动态调整阈值：
    /// - 增仓方向 → 阈值提高（更难触发，控制风险）
    /// - 减仓方向 → 阈值降低（更易触发，降低风险）
    ///
    /// 公式: effective_threshold = base * (1 + direction_factor * leverage_ratio)
    /// - direction_factor：同向/新开仓 = +1，反向 = -1
    /// - leverage_ratio 已 clamp 到 [0, 1]，故阈值恒落在 [0, 2 * base]
    ///
    /// **阈值不允许为负**：负阈值意味着偏离度为负（卖在均值下方、买在均值上方）也照单成交，
    /// 那是确定性亏损的成交，不能以"便于减仓"为由放开。
    fn calculate_effective_threshold(
        &self,
        short_exchange: Exchange,
        long_exchange: Exchange,
        state: &SymbolState,
        state_manager: &StateManager,
    ) -> f64 {
        let base_threshold = self.config.deviation_threshold;

        // 计算单个交易所的 symbol 杠杆率
        // 数据不足时返回 0.0（无仓位或无 equity 时杠杆率为零，安全侧）
        // BBO 不存在时无法计算仓位价值，返回 0.0（保守：不会误触阈值放行）
        let calc_leverage = |exchange: Exchange| -> f64 {
            let Some(equity) = state_manager.equity(exchange) else {
                return 0.0;
            };
            if equity <= 0.0 {
                return 0.0;
            }
            let pos_size = state.position_size(exchange).abs();
            let Some(bbo) = state.bbo(exchange) else {
                return 0.0;
            };
            (pos_size * bbo.mid_price()) / equity
        };

        let short_leverage = calc_leverage(short_exchange);
        let long_leverage = calc_leverage(long_exchange);

        // 获取仓位（带符号）
        let short_pos = state.position_size(short_exchange);
        let long_pos = state.position_size(long_exchange);

        // 方向因子：仓位与订单方向同向（增仓）为 +1，反向（减仓）为 -1
        // long 腿下买单，short 腿下卖单
        let long_factor = direction_factor(long_pos, Side::Long);
        let short_factor = direction_factor(short_pos, Side::Short);

        // 以杠杆率高的一边为参考
        let (raw_leverage, factor) = if short_leverage >= long_leverage {
            (short_leverage, short_factor)
        } else {
            (long_leverage, long_factor)
        };
        let ratio = leverage_ratio(raw_leverage, self.config.max_symbol_leverage);

        // 下界取 ioc_slippage：让价幅度是代码里唯一可见的确定成本，阈值低于它就必然亏。
        // 与 SpreadArbConfig::validate 拒绝 `ioc_slippage >= deviation_threshold` 是同一条口径——
        // 启动期判为负期望的配置，运行期也不能因为"想减仓"而放行。
        let effective_threshold =
            (base_threshold * (1.0 + factor * ratio)).max(self.config.ioc_slippage);

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
            direction_factor = format!("{:.1}", factor),
            leverage_ratio = format!("{:.4}", ratio),
            effective_threshold = format!("{:.4}", effective_threshold),
            "Calculated effective threshold"
        );

        effective_threshold
    }

    // ========== Pipeline 处理 ==========

    /// Pipeline：合法性检查
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

    /// Pipeline：单边（symbol 级）杠杆率检查
    ///
    /// 用**带符号**仓位推算下单后的新仓位：long 腿 `pos + size`，short 腿 `pos - size`。
    /// 只有 |新仓位| > |原仓位|（增仓方向）且新杠杆率超限才拦截——减仓单永不被拦截。
    ///
    /// 早期实现用 `pos.abs() + size` 推算，会把"在多头仓位上卖出"误判成增仓，
    /// 导致杠杆越接近上限、越需要减仓时，减仓单反而被挡死。
    fn check_symbol_leverage(
        &self,
        signal: &mut TradingSignal,
        state: &SymbolState,
        state_manager: &StateManager,
    ) {
        let Some((short_equity, long_equity)) =
            self.require_equities(signal, state_manager, "symbol_leverage")
        else {
            return;
        };

        let short_pos = state.position_size(signal.short_exchange);
        let long_pos = state.position_size(signal.long_exchange);

        let new_short_pos = new_position(short_pos, Side::Short, signal.short_size);
        let new_long_pos = new_position(long_pos, Side::Long, signal.long_size);

        // 各腿用**自己**交易所的价格估值：两所价差正是本策略的信号来源，
        // 用两所均价给单腿估值会带入系统性偏差
        let new_short_leverage = (new_short_pos.abs() * signal.short_price) / short_equity;
        let new_long_leverage = (new_long_pos.abs() * signal.long_price) / long_equity;

        // 边界口径与 check_position_limit / check_account_leverage 统一：恰好打满上限放行，超出才拦
        let short_blocked = is_increasing(short_pos, new_short_pos)
            && new_short_leverage > self.config.max_symbol_leverage;
        let long_blocked = is_increasing(long_pos, new_long_pos)
            && new_long_leverage > self.config.max_symbol_leverage;

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
                short_pos = format!("{:.4}", short_pos),
                new_short_leverage = format!("{:.4}", new_short_leverage),
                short_blocked = short_blocked,
                long_exchange = %signal.long_exchange,
                long_pos = format!("{:.4}", long_pos),
                new_long_leverage = format!("{:.4}", new_long_leverage),
                long_blocked = long_blocked,
                max_symbol_leverage = format!("{:.4}", self.config.max_symbol_leverage),
                "Signal adjusted: symbol leverage exceeds threshold"
            );
        }
    }

    /// Pipeline：单 pair 仓位名义上限
    ///
    /// 单 symbol 在单个交易所的持仓名义价值上限（USDT，绝对值）。与杠杆检查同构：
    /// 只拦截增仓方向，减仓永不拦截。这是替代"下单限速"的风险闸门——限制的是最终暴露，
    /// 而不是下单频率。
    fn check_position_limit(&self, signal: &mut TradingSignal, state: &SymbolState) {
        if signal.short_price <= 0.0 || signal.long_price <= 0.0 {
            return;
        }

        let short_pos = state.position_size(signal.short_exchange);
        let long_pos = state.position_size(signal.long_exchange);

        let new_short_pos = new_position(short_pos, Side::Short, signal.short_size);
        let new_long_pos = new_position(long_pos, Side::Long, signal.long_size);

        let limit = self.config.max_position_notional;
        // 各腿用自己交易所的价格估值（同 check_symbol_leverage）
        let short_new_notional = new_short_pos.abs() * signal.short_price;
        let long_new_notional = new_long_pos.abs() * signal.long_price;

        let short_blocked =
            is_increasing(short_pos, new_short_pos) && short_new_notional > limit;
        let long_blocked = is_increasing(long_pos, new_long_pos) && long_new_notional > limit;

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
                short_new_notional = format!("{:.2}", short_new_notional),
                short_blocked = short_blocked,
                long_exchange = %signal.long_exchange,
                long_new_notional = format!("{:.2}", long_new_notional),
                long_blocked = long_blocked,
                max_position_notional = format!("{:.2}", limit),
                "Signal adjusted: pair position notional exceeds limit"
            );
        }
    }

    /// Pipeline：账户杠杆率检查
    ///
    /// 检查账户级别杠杆率 (account_notional / equity)。超过阈值时拦截**增仓方向**的腿。
    ///
    /// 早期实现的判据是"本 symbol 在该所已有同向仓位"，这在只跑两三个 symbol 时够用，但在
    /// 按桶跑数百 symbol 时是致命漏洞：本 symbol 空仓即永不拦截，于是账户杠杆可以靠"不断开新
    /// symbol"无限增长——单 symbol 上界受 max_position_notional / max_symbol_leverage 约束，
    /// 账户总量却没有任何闸门。且跨所对冲在单个交易所内是**单边方向暴露**（一所全多、另一所
    /// 全空），两边不能互相提供保证金，单边行情即单所强平。
    /// 改为按"这笔单是否增仓"判定（空仓开新仓也是增仓），与单边杠杆闸门口径一致。
    fn check_account_leverage(
        &self,
        signal: &mut TradingSignal,
        state: &SymbolState,
        state_manager: &StateManager,
    ) {
        let Some((short_equity, long_equity)) =
            self.require_equities(signal, state_manager, "account_leverage")
        else {
            return;
        };

        // 计算两边交易所的账户杠杆率
        // account_info 原子包含 equity + notional，上面已通过 equity 检查确认数据已到达。
        // 若此处仍缺失（不应发生），保守处理：本轮不下单，记录后返回，不 panic。
        let (Some(short_ai), Some(long_ai)) = (
            state_manager.account_info(signal.short_exchange),
            state_manager.account_info(signal.long_exchange),
        ) else {
            tracing::warn!(
                symbol = %self.symbol,
                "account_info 缺失（equity 已到但 notional 未到），本轮不下单"
            );
            signal.long_size = 0.0;
            signal.short_size = 0.0;
            return;
        };
        let short_leverage = short_ai.notional / short_equity;
        let long_leverage = long_ai.notional / long_equity;

        // 获取当前 symbol 在各交易所的仓位
        let short_pos = state.position_size(signal.short_exchange);
        let long_pos = state.position_size(signal.long_exchange);

        let new_short_pos = new_position(short_pos, Side::Short, signal.short_size);
        let new_long_pos = new_position(long_pos, Side::Long, signal.long_size);

        // 账户杠杆超标 → 拦截增仓方向的腿（减仓永不拦截）
        let short_blocked = short_leverage > self.config.max_account_leverage
            && is_increasing(short_pos, new_short_pos);
        let long_blocked = long_leverage > self.config.max_account_leverage
            && is_increasing(long_pos, new_long_pos);

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

    /// 取两条腿的 equity；任一缺失或非正则清零 signal 并返回 None
    ///
    /// 两个杠杆检查步骤共用同一前置条件，抽出以消除重复。
    fn require_equities(
        &self,
        signal: &mut TradingSignal,
        state_manager: &StateManager,
        stage: &str,
    ) -> Option<(f64, f64)> {
        let (Some(short_equity), Some(long_equity)) = (
            state_manager.equity(signal.short_exchange),
            state_manager.equity(signal.long_exchange),
        ) else {
            tracing::warn!(
                symbol = %self.symbol,
                stage = stage,
                short_exchange = %signal.short_exchange,
                long_exchange = %signal.long_exchange,
                "Equity not yet available, skipping order"
            );
            signal.long_size = 0.0;
            signal.short_size = 0.0;
            return None;
        };

        if short_equity <= 0.0 || long_equity <= 0.0 {
            tracing::warn!(
                symbol = %self.symbol,
                stage = stage,
                short_exchange = %signal.short_exchange,
                short_equity = short_equity,
                long_exchange = %signal.long_exchange,
                long_equity = long_equity,
                "Insufficient equity"
            );
            signal.long_size = 0.0;
            signal.short_size = 0.0;
            return None;
        }

        Some((short_equity, long_equity))
    }

    /// Pipeline：净敞口修正
    ///
    /// 根据当前净敞口调整下单数量。例如净敞口为 +10（多头多），则多头下单量减去 10（取 max(0)）。
    /// 这是本策略**唯一**的敞口收敛机制（不做强制 rebalance）：单腿成交造成的裸敞口靠后续
    /// 信号逐步中和。
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

    /// Pipeline：notional 限制
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

    /// Pipeline：单腿保护
    ///
    /// 前面的风控闸门是逐腿判定的，可能只清零一条腿。此时剩下的单腿单有两种性质：
    /// - **收敛净敞口**（例如敞口修正故意只开一条腿去中和）→ 放行，这是敞口收敛机制本身
    /// - **凭空制造方向暴露**（例如某腿被仓位上限挡住，另一腿照开）→ 必须一起清零
    ///
    /// 判据只能是"是否收敛 symbol 的净敞口"，不能是"是否减少该交易所自身仓位"：
    /// 中和敞口的那条腿通常是在另一个所**新开**仓位（自身仓位变大），但净敞口在缩小。
    fn enforce_single_leg_reduces_exposure(&self, signal: &mut TradingSignal, state: &SymbolState) {
        let short_active = signal.short_size > POSITION_EPSILON;
        let long_active = signal.long_size > POSITION_EPSILON;

        // 两腿齐发或两腿全无，都无需干预
        if short_active == long_active {
            return;
        }

        let (long_pos, short_pos) = state.position_sizes();
        let net_exposure = long_pos + short_pos;
        let delta = if long_active {
            signal.long_size
        } else {
            -signal.short_size
        };
        let new_net_exposure = net_exposure + delta;

        if new_net_exposure.abs() > net_exposure.abs() + POSITION_EPSILON {
            tracing::info!(
                symbol = %self.symbol,
                net_exposure = format!("{:.6}", net_exposure),
                new_net_exposure = format!("{:.6}", new_net_exposure),
                long_size = signal.long_size,
                short_size = signal.short_size,
                "Signal dropped: 单腿单会放大净敞口（另一腿已被风控拦截）"
            );
            signal.long_size = 0.0;
            signal.short_size = 0.0;
        }
    }

    /// 运行完整的信号处理 pipeline
    ///
    /// 顺序：validate → 净敞口修正 → notional 限制 → 账户杠杆 → 单边杠杆 → 单 pair 仓位
    /// → 单腿保护
    ///
    /// 三个"增仓拦截"步骤都排在 notional 限制之后：`is_increasing` 对下单量不是单调的
    /// （在多头上卖 1 是减仓、卖 11 就成了反向增仓），必须基于**最终**下单量判定。
    /// 单腿保护必须最后跑，它要看的是所有闸门作用完之后的形态。
    /// 各步骤通过将 size 设为 0 表示该侧不下单。
    fn process_signal(
        &self,
        mut signal: TradingSignal,
        state: &SymbolState,
        state_manager: &StateManager,
    ) -> TradingSignal {
        self.validate_signal(&mut signal);
        self.adjust_for_exposure(&mut signal, state);
        self.set_notional_limits(&mut signal);
        self.check_account_leverage(&mut signal, state, state_manager);
        self.check_symbol_leverage(&mut signal, state, state_manager);
        self.check_position_limit(&mut signal, state);
        self.enforce_single_leg_reduces_exposure(&mut signal, state);
        signal
    }

    /// 根据处理后的信号生成订单列表
    fn make_orders(&self, signal: &TradingSignal) -> Vec<Order> {
        // 计算带滑点的价格（模拟市价单）
        // short_price 是 bid，做空用 bid - slippage
        // long_price 是 ask，做多用 ask + slippage
        let short_limit_price = signal.short_price * (1.0 - self.config.ioc_slippage);
        let long_limit_price = signal.long_price * (1.0 + self.config.ioc_slippage);

        let mut orders = Vec::new();

        if signal.short_size > POSITION_EPSILON {
            orders.push(Order {
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
            });
        }

        if signal.long_size > POSITION_EPSILON {
            orders.push(Order {
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
            });
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

// ========== 纯函数（风控算术，独立可测） ==========

/// 下单后的新仓位（带符号）：买单 `pos + size`，卖单 `pos - size`
fn new_position(pos: f64, side: Side, size: f64) -> f64 {
    match side {
        Side::Long => pos + size,
        Side::Short => pos - size,
    }
}

/// 该笔订单是否为增仓方向（绝对仓位变大）
fn is_increasing(pos: f64, new_pos: f64) -> bool {
    new_pos.abs() > pos.abs() + POSITION_EPSILON
}

/// 仓位与订单方向的同向系数：同向（增仓）+1，反向（减仓）-1
///
/// 空仓视为 +1（新开仓按增仓对待，阈值提高）。
fn direction_factor(pos: f64, side: Side) -> f64 {
    let signed = match side {
        Side::Long => pos,
        Side::Short => -pos,
    };
    if signed < -POSITION_EPSILON {
        -1.0
    } else {
        1.0
    }
}

/// 杠杆率占上限的比例，clamp 到 [0, 1]
///
/// 上限非正、或输入非有限时返回 0（保守：阈值退化为 base，既不放行也不加码）。
/// clamp 上界是关键：没有它，超限仓位会把动态阈值压成负数，导致"偏离度为负也成交"。
fn leverage_ratio(leverage: f64, max_leverage: f64) -> f64 {
    if max_leverage <= 0.0 || !leverage.is_finite() || !max_leverage.is_finite() {
        return 0.0;
    }
    (leverage / max_leverage).clamp(0.0, 1.0)
}

impl Strategy for SpreadArbStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        // 纯价差套利只需要 BBO 数据
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
        let Some(symbol_state) = state.symbol_state(&self.symbol) else {
            return vec![];
        };

        let now = event.local_ts;

        // BBO 事件时更新该交易所的 bid/ask EMA 与本地接收时刻
        if let ExchangeEventData::BBO(bbo) = &event.data {
            // 顺序关键：先按"距上一条 BBO 的间隔"判定是否中断恢复并重置 EMA，
            // 再更新 EMA 与接收时刻——否则时刻已被刷新，就判不出中断了。
            self.reset_ema_if_gap_exceeded(bbo.exchange, now);
            self.update_exchange_ema(bbo.exchange, symbol_state);
            self.bbo_local_ts.insert(bbo.exchange, now);
        }

        // 行情新鲜度状态变迁日志（Clock 事件也会推进，行情停推时能报警）
        self.log_staleness_transitions(now);

        // 不做"所有交易所 EMA 都预热完成"的全局门：预热与新鲜度都已在候选筛选阶段逐所过滤
        // （见 check_signal），全局 AND 会让任一交易所长期无报价就冻结整个 symbol。

        // 有未完成订单时等待
        if symbol_state.has_pending_orders() {
            return vec![];
        }

        // 检查信号并通过 pipeline 处理
        if let Some(signal) = self.check_signal(symbol_state, state, now) {
            let processed = self.process_signal(signal, symbol_state, state);
            let orders = self.make_orders(&processed);
            if !orders.is_empty() {
                let (long_size, short_size) = symbol_state.position_sizes();
                let net_exposure = long_size + short_size;
                let comment = format!(
                    "deviation | short={} long={} | s_dev={:.4}% l_dev={:.4}% | exp={:.4}",
                    processed.short_exchange,
                    processed.long_exchange,
                    processed.short_deviation * 100.0,
                    processed.long_deviation * 100.0,
                    net_exposure,
                );
                return vec![OutcomeEvent::PlaceOrders { orders, comment }];
            }
        }

        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Position, BBO};
    use crate::messaging::ExchangeEventData;

    const SYMBOL: &str = "BTC";
    const SHORT_EX: Exchange = Exchange::Binance;
    const LONG_EX: Exchange = Exchange::OKX;

    fn config() -> SpreadArbConfig {
        SpreadArbConfig {
            ema_period: 3,
            deviation_threshold: 0.004,
            max_notional: 1000.0,
            min_notional: 10.0,
            order_timeout_ms: 10_000,
            max_position_notional: 5000.0,
            max_symbol_leverage: 0.5,
            max_account_leverage: 3.0,
            ioc_slippage: 0.001,
            max_bbo_age_ms: 3000,
        }
    }

    fn strategy(config: SpreadArbConfig) -> SpreadArbStrategy {
        SpreadArbStrategy::new(config, vec![SHORT_EX, LONG_EX], SYMBOL.to_string())
    }

    fn bbo(exchange: Exchange, bid: f64, ask: f64, qty: f64) -> BBO {
        BBO {
            exchange,
            symbol: SYMBOL.to_string(),
            bid_price: bid,
            bid_qty: qty,
            ask_price: ask,
            ask_qty: qty,
            timestamp: 0,
        }
    }

    /// 构造 SymbolState：两所各一条 BBO + 指定仓位
    fn symbol_state(positions: &[(Exchange, f64)]) -> SymbolState {
        let mut state = SymbolState::new(SYMBOL.to_string());
        state.bbos.insert(SHORT_EX, bbo(SHORT_EX, 100.0, 100.2, 10.0));
        state.bbos.insert(LONG_EX, bbo(LONG_EX, 99.8, 100.0, 10.0));
        for &(exchange, size) in positions {
            state.positions.insert(
                exchange,
                Position {
                    exchange,
                    symbol: SYMBOL.to_string(),
                    size,
                },
            );
        }
        state
    }

    /// 构造 StateManager 并喂入两所的 AccountInfo
    fn state_manager(equity: f64, notional: f64) -> StateManager {
        let mut manager = StateManager::new(&[SYMBOL.to_string()], 10_000);
        for exchange in [SHORT_EX, LONG_EX] {
            manager.apply(&IncomeEvent {
                exchange_ts: 0,
                local_ts: 0,
                data: ExchangeEventData::AccountInfo {
                    exchange,
                    equity,
                    notional,
                },
            });
        }
        manager
    }

    fn signal(long_size: f64, short_size: f64) -> TradingSignal {
        TradingSignal {
            long_exchange: LONG_EX,
            long_price: 100.0,
            long_size,
            short_exchange: SHORT_EX,
            short_price: 100.0,
            short_size,
            long_deviation: 0.0,
            short_deviation: 0.0,
        }
    }

    /// 走完整事件路径（先更新 StateManager，再喂策略）让两所 EMA 预热完成
    ///
    /// 必须同时更新 manager：策略的 EMA 取值来自 `StateManager` 里的 SymbolState，
    /// 只发事件不落状态是不成立的路径。
    fn warm_up(strategy: &mut SpreadArbStrategy, manager: &mut StateManager, local_ts: Timestamp) {
        for _ in 0..config().ema_period {
            for exchange in [SHORT_EX, LONG_EX] {
                let (bid, ask) = if exchange == SHORT_EX {
                    (100.0, 100.2)
                } else {
                    (99.8, 100.0)
                };
                let event = IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::BBO(bbo(exchange, bid, ask, 10.0)),
                };
                manager.apply(&event);
                strategy.on_event(&event, manager);
            }
        }
    }

    /// 显著错价：SHORT_EX 的 bid 被抬高、LONG_EX 的 ask 被压低
    fn mispriced_state() -> SymbolState {
        let mut state = symbol_state(&[]);
        state.bbos.insert(SHORT_EX, bbo(SHORT_EX, 110.0, 110.2, 10.0));
        state.bbos.insert(LONG_EX, bbo(LONG_EX, 89.8, 90.0, 10.0));
        state
    }

    // ===== 纯函数 =====

    #[test]
    fn new_position_uses_signed_arithmetic() {
        assert_eq!(new_position(5.0, Side::Short, 1.0), 4.0);
        assert_eq!(new_position(-5.0, Side::Short, 1.0), -6.0);
        assert_eq!(new_position(-5.0, Side::Long, 1.0), -4.0);
        assert_eq!(new_position(5.0, Side::Long, 1.0), 6.0);
    }

    #[test]
    fn is_increasing_only_when_absolute_position_grows() {
        assert!(is_increasing(5.0, 6.0));
        assert!(is_increasing(-5.0, -6.0));
        assert!(!is_increasing(5.0, 4.0));
        assert!(!is_increasing(-5.0, -4.0));
        assert!(!is_increasing(0.0, 0.0));
        // 反手穿越零点：|新| 更小仍算减仓
        assert!(!is_increasing(5.0, -4.0));
    }

    #[test]
    fn direction_factor_treats_flat_as_opening() {
        assert_eq!(direction_factor(0.0, Side::Long), 1.0);
        assert_eq!(direction_factor(0.0, Side::Short), 1.0);
        // 多头上买 = 加仓，多头上卖 = 减仓
        assert_eq!(direction_factor(5.0, Side::Long), 1.0);
        assert_eq!(direction_factor(5.0, Side::Short), -1.0);
        // 空头上卖 = 加仓，空头上买 = 减仓
        assert_eq!(direction_factor(-5.0, Side::Short), 1.0);
        assert_eq!(direction_factor(-5.0, Side::Long), -1.0);
    }

    #[test]
    fn leverage_ratio_is_clamped_and_guards_bad_input() {
        assert_eq!(leverage_ratio(0.25, 0.5), 0.5);
        // 超限时 clamp 到 1，保证阈值不会变负
        assert_eq!(leverage_ratio(1.5, 0.5), 1.0);
        assert_eq!(leverage_ratio(-1.0, 0.5), 0.0);
        assert_eq!(leverage_ratio(0.5, 0.0), 0.0);
        assert_eq!(leverage_ratio(f64::NAN, 0.5), 0.0);
        assert_eq!(leverage_ratio(f64::INFINITY, 0.5), 0.0);
    }

    // ===== 动态阈值 =====

    #[test]
    fn threshold_never_goes_negative_when_leverage_exceeds_limit() {
        let strategy = strategy(config());
        // 两所都持有多头 → short 腿是减仓方向（factor = -1）
        // 仓位 100 × 100 = 10000 名义 / equity 1000 = 杠杆 10，远超上限 0.5
        let state = symbol_state(&[(SHORT_EX, 100.0), (LONG_EX, 100.0)]);
        let manager = state_manager(1000.0, 0.0);

        let threshold =
            strategy.calculate_effective_threshold(SHORT_EX, LONG_EX, &state, &manager);

        // ratio clamp 到 1 → base * (1 - 1) = 0，再由 ioc_slippage 托底
        assert!(
            threshold >= config().ioc_slippage,
            "阈值不能低于让价成本，否则会在亏损价位成交: {threshold}"
        );
    }

    #[test]
    fn threshold_doubles_at_most_when_adding_to_position() {
        let strategy = strategy(config());
        // 空头仓位在 short 腿 → short 腿继续卖是加仓（factor = +1）
        let state = symbol_state(&[(SHORT_EX, -100.0)]);
        let manager = state_manager(1000.0, 0.0);

        let threshold =
            strategy.calculate_effective_threshold(SHORT_EX, LONG_EX, &state, &manager);

        assert!((threshold - 2.0 * config().deviation_threshold).abs() < 1e-12);
    }

    #[test]
    fn threshold_equals_base_when_flat() {
        let strategy = strategy(config());
        let state = symbol_state(&[]);
        let manager = state_manager(1000.0, 0.0);

        let threshold =
            strategy.calculate_effective_threshold(SHORT_EX, LONG_EX, &state, &manager);

        assert!((threshold - config().deviation_threshold).abs() < 1e-12);
    }

    // ===== 单边杠杆检查 =====

    #[test]
    fn symbol_leverage_does_not_block_reducing_orders() {
        let strategy = strategy(config());
        // short 腿持有多头 5（名义 500 / equity 1000 = 0.5，已到上限）
        // 在其上卖出 1 是减仓，必须放行
        let state = symbol_state(&[(SHORT_EX, 5.0)]);
        let manager = state_manager(1000.0, 0.0);
        let mut sig = signal(0.0, 1.0);

        strategy.check_symbol_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.short_size, 1.0, "减仓单被误挡");
    }

    #[test]
    fn symbol_leverage_blocks_increasing_orders_over_limit() {
        let strategy = strategy(config());
        // short 腿已有空头 5（名义 500 / equity 1000 = 0.5 = 上限），继续卖出是加仓
        let state = symbol_state(&[(SHORT_EX, -5.0)]);
        let manager = state_manager(1000.0, 0.0);
        let mut sig = signal(0.0, 1.0);

        strategy.check_symbol_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.short_size, 0.0, "超限加仓单未被拦截");
    }

    #[test]
    fn symbol_leverage_blocks_only_the_offending_leg() {
        let strategy = strategy(config());
        // short 腿超限加仓；long 腿空仓、下单量小，应放行
        let state = symbol_state(&[(SHORT_EX, -5.0)]);
        let manager = state_manager(1000.0, 0.0);
        let mut sig = signal(0.5, 1.0);

        strategy.check_symbol_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.short_size, 0.0);
        assert_eq!(sig.long_size, 0.5);
    }

    #[test]
    fn missing_equity_zeroes_both_legs() {
        let strategy = strategy(config());
        let state = symbol_state(&[]);
        // 未喂 AccountInfo → equity 缺失
        let manager = StateManager::new(&[SYMBOL.to_string()], 10_000);
        let mut sig = signal(1.0, 1.0);

        strategy.check_symbol_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.long_size, 0.0);
        assert_eq!(sig.short_size, 0.0);
    }

    // ===== 单 pair 仓位上限 =====

    #[test]
    fn position_limit_blocks_increase_beyond_cap() {
        let cfg = SpreadArbConfig {
            max_position_notional: 1000.0,
            ..config()
        };
        let strategy = strategy(cfg);
        // short 腿已有空头 9.5（名义 950），再卖 1 → 1050 > 1000 上限
        let state = symbol_state(&[(SHORT_EX, -9.5)]);
        let mut sig = signal(0.0, 1.0);

        strategy.check_position_limit(&mut sig, &state);

        assert_eq!(sig.short_size, 0.0);
    }

    #[test]
    fn position_limit_allows_increase_within_cap() {
        let cfg = SpreadArbConfig {
            max_position_notional: 1000.0,
            ..config()
        };
        let strategy = strategy(cfg);
        let state = symbol_state(&[(SHORT_EX, -5.0)]);
        let mut sig = signal(0.0, 1.0);

        strategy.check_position_limit(&mut sig, &state);

        assert_eq!(sig.short_size, 1.0);
    }

    #[test]
    fn position_limit_never_blocks_reduction() {
        let cfg = SpreadArbConfig {
            max_position_notional: 100.0,
            ..config()
        };
        let strategy = strategy(cfg);
        // 已远超上限（名义 5000），但卖出是在多头上减仓 → 必须放行
        let state = symbol_state(&[(SHORT_EX, 50.0)]);
        let mut sig = signal(0.0, 1.0);

        strategy.check_position_limit(&mut sig, &state);

        assert_eq!(sig.short_size, 1.0);
    }

    // ===== 敞口修正 / notional =====

    #[test]
    fn exposure_adjustment_shrinks_the_overweight_side() {
        let strategy = strategy(config());
        // 净敞口 +2（多头多）→ 多头下单量减 2，空头正常
        let state = symbol_state(&[(LONG_EX, 3.0), (SHORT_EX, -1.0)]);
        let mut sig = signal(5.0, 5.0);

        strategy.adjust_for_exposure(&mut sig, &state);

        assert!((sig.long_size - 3.0).abs() < 1e-9);
        assert!((sig.short_size - 5.0).abs() < 1e-9);
    }

    #[test]
    fn exposure_adjustment_clamps_at_zero() {
        let strategy = strategy(config());
        // 净敞口 +10 远大于基础量 → 多头侧归零，只开空头中和
        let state = symbol_state(&[(LONG_EX, 10.0), (SHORT_EX, 0.0)]);
        let mut sig = signal(2.0, 2.0);

        strategy.adjust_for_exposure(&mut sig, &state);

        assert_eq!(sig.long_size, 0.0);
        assert!((sig.short_size - 2.0).abs() < 1e-9);
    }

    #[test]
    fn exposure_adjustment_is_symmetric_for_short_overweight() {
        let strategy = strategy(config());
        let state = symbol_state(&[(SHORT_EX, -3.0), (LONG_EX, 1.0)]);
        let mut sig = signal(5.0, 5.0);

        strategy.adjust_for_exposure(&mut sig, &state);

        assert!((sig.long_size - 5.0).abs() < 1e-9);
        assert!((sig.short_size - 3.0).abs() < 1e-9);
    }

    #[test]
    fn notional_limits_zero_dust_and_cap_oversize() {
        let strategy = strategy(config());
        // min_notional 10 / price 100 = 0.1；max_notional 1000 / 100 = 10
        let mut sig = signal(0.05, 50.0);

        strategy.set_notional_limits(&mut sig);

        assert_eq!(sig.long_size, 0.0, "低于 min_notional 应归零");
        assert!((sig.short_size - 10.0).abs() < 1e-9, "超过 max_notional 应截断");
    }

    #[test]
    fn validate_signal_rejects_bad_prices() {
        let strategy = strategy(config());
        let mut sig = TradingSignal {
            long_price: 0.0,
            ..signal(1.0, 1.0)
        };

        strategy.validate_signal(&mut sig);

        assert_eq!(sig.long_size, 0.0);
        assert_eq!(sig.short_size, 0.0);
    }

    // ===== EMA 预热 / 新鲜度 =====

    #[test]
    fn no_signal_before_emas_are_warm() {
        let mut strategy = strategy(config());
        let mut manager = state_manager(1000.0, 0.0);
        let event = IncomeEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: ExchangeEventData::BBO(bbo(SHORT_EX, 100.0, 100.2, 10.0)),
        };
        manager.apply(&event);

        // 只喂一条，远未满 ema_period
        let outcome = strategy.on_event(&event, &manager);

        assert!(outcome.is_empty());
        let state = symbol_state(&[]);
        assert!(strategy.bid_deviation(SHORT_EX, &state).is_none());
    }

    #[test]
    fn deviations_available_after_warm_up() {
        let mut strategy = strategy(config());
        let mut manager = state_manager(1000.0, 0.0);
        warm_up(&mut strategy, &mut manager, 0);

        let state = symbol_state(&[]);
        for exchange in [SHORT_EX, LONG_EX] {
            assert!(strategy.bid_deviation(exchange, &state).is_some());
            assert!(strategy.ask_deviation(exchange, &state).is_some());
        }
    }

    /// 一个所长期无报价时，其余所仍应能配对（不再有全所 AND 门）
    #[test]
    fn one_unwarmed_exchange_does_not_freeze_the_symbol() {
        let cfg = config();
        let mut strategy = SpreadArbStrategy::new(
            cfg.clone(),
            vec![SHORT_EX, LONG_EX, Exchange::Hyperliquid],
            SYMBOL.to_string(),
        );
        let mut manager = state_manager(100_000.0, 0.0);
        // 只预热两个所，Hyperliquid 从未有过行情
        warm_up(&mut strategy, &mut manager, 0);

        let state = mispriced_state();
        strategy.bbo_local_ts.insert(SHORT_EX, 0);
        strategy.bbo_local_ts.insert(LONG_EX, 0);

        assert!(
            strategy.check_signal(&state, &manager, 0).is_some(),
            "第三个所无报价不应冻结另两个所的配对"
        );
    }

    #[test]
    fn stale_bbo_is_not_fresh() {
        let mut strategy = strategy(config());
        strategy.bbo_local_ts.insert(SHORT_EX, 1_000);

        assert!(strategy.is_bbo_fresh(SHORT_EX, 1_000 + config().max_bbo_age_ms));
        assert!(!strategy.is_bbo_fresh(SHORT_EX, 1_000 + config().max_bbo_age_ms + 1));
        // 从未收到过 BBO 的交易所永远视为不新鲜
        assert!(!strategy.is_bbo_fresh(LONG_EX, 1_000));
    }

    #[test]
    fn stale_exchange_produces_no_signal() {
        let cfg = config();
        let mut strategy = strategy(cfg.clone());
        let mut manager = state_manager(100_000.0, 0.0);
        warm_up(&mut strategy, &mut manager, 0);

        // 错价足够大，新鲜时必然出信号；唯一变量是行情年龄
        let state = mispriced_state();
        strategy.bbo_local_ts.insert(SHORT_EX, 0);
        strategy.bbo_local_ts.insert(LONG_EX, 0);

        assert!(
            strategy.check_signal(&state, &manager, cfg.max_bbo_age_ms).is_some(),
            "窗口内应出信号"
        );
        assert!(
            strategy
                .check_signal(&state, &manager, cfg.max_bbo_age_ms + 1)
                .is_none(),
            "行情超龄后不得下单"
        );
    }

    #[test]
    fn one_stale_leg_blocks_the_pair() {
        let cfg = config();
        let mut strategy = strategy(cfg.clone());
        let mut manager = state_manager(100_000.0, 0.0);
        warm_up(&mut strategy, &mut manager, 0);

        let state = mispriced_state();
        // 只有 SHORT_EX 新鲜；两所都需新鲜才能配对
        strategy.bbo_local_ts.insert(SHORT_EX, cfg.max_bbo_age_ms);
        strategy.bbo_local_ts.insert(LONG_EX, 0);

        assert!(strategy
            .check_signal(&state, &manager, cfg.max_bbo_age_ms + 1)
            .is_none());
    }

    // ===== 账户杠杆闸门（回归：空仓 symbol 曾能绕过账户杠杆上限） =====

    #[test]
    fn account_leverage_blocks_opening_on_a_flat_symbol() {
        let strategy = strategy(config());
        // 账户杠杆 = notional/equity = 5000/1000 = 5 > 上限 3
        // 本 symbol 在两所都空仓——旧实现此时完全不拦，账户杠杆可靠"开新 symbol"无限增长
        let state = symbol_state(&[]);
        let manager = state_manager(1000.0, 5000.0);
        let mut sig = signal(1.0, 1.0);

        strategy.check_account_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.long_size, 0.0, "账户杠杆超限时不得在空仓 symbol 上开新仓");
        assert_eq!(sig.short_size, 0.0);
    }

    #[test]
    fn account_leverage_allows_reducing_when_over_limit() {
        let strategy = strategy(config());
        // 账户杠杆超限，但两条腿都是减仓方向：short 腿在多头上卖、long 腿在空头上买
        let state = symbol_state(&[(SHORT_EX, 5.0), (LONG_EX, -5.0)]);
        let manager = state_manager(1000.0, 5000.0);
        let mut sig = signal(1.0, 1.0);

        strategy.check_account_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.short_size, 1.0);
        assert_eq!(sig.long_size, 1.0);
    }

    #[test]
    fn account_leverage_under_limit_does_not_block() {
        let strategy = strategy(config());
        let state = symbol_state(&[]);
        // 杠杆 1.0 < 上限 3
        let manager = state_manager(1000.0, 1000.0);
        let mut sig = signal(1.0, 1.0);

        strategy.check_account_leverage(&mut sig, &state, &manager);

        assert_eq!(sig.long_size, 1.0);
        assert_eq!(sig.short_size, 1.0);
    }

    // ===== 行情中断恢复（回归：恢复第一跳曾用陈旧 EMA 产生假偏离） =====

    #[test]
    fn ema_is_reset_after_a_data_gap() {
        let cfg = config();
        let mut strategy = strategy(cfg.clone());
        let mut manager = state_manager(100_000.0, 0.0);
        warm_up(&mut strategy, &mut manager, 0);

        // 中断后恢复：价格已整体上移 10%，但 EMA 还停在 100 附近
        let gap_end = cfg.max_bbo_age_ms + 1;
        let recovered = bbo(SHORT_EX, 110.0, 110.2, 10.0);
        let event = IncomeEvent {
            exchange_ts: gap_end,
            local_ts: gap_end,
            data: ExchangeEventData::BBO(recovered),
        };
        manager.apply(&event);
        let outcome = strategy.on_event(&event, &manager);

        assert!(
            outcome.is_empty(),
            "中断恢复的第一跳不得被当成错价下单"
        );
        // EMA 已重置 → 该所退出候选，直到重新预热
        let state = symbol_state(&[]);
        assert!(strategy.bid_deviation(SHORT_EX, &state).is_none());
        // 未中断的另一所不受影响
        assert!(strategy.bid_deviation(LONG_EX, &state).is_some());
    }

    #[test]
    fn ema_survives_normal_tick_intervals() {
        let cfg = config();
        let mut strategy = strategy(cfg.clone());
        let mut manager = state_manager(100_000.0, 0.0);
        warm_up(&mut strategy, &mut manager, 0);

        // 间隔恰好等于上限，属正常范围，不应重置
        let event = IncomeEvent {
            exchange_ts: cfg.max_bbo_age_ms,
            local_ts: cfg.max_bbo_age_ms,
            data: ExchangeEventData::BBO(bbo(SHORT_EX, 100.0, 100.2, 10.0)),
        };
        manager.apply(&event);
        strategy.on_event(&event, &manager);

        let state = symbol_state(&[]);
        assert!(strategy.bid_deviation(SHORT_EX, &state).is_some());
    }

    // ===== 单腿保护 =====

    #[test]
    fn single_leg_that_would_grow_exposure_is_dropped() {
        let strategy = strategy(config());
        // 完全对冲（净敞口 0），只剩 short 腿 → 会造出 -1 的方向暴露
        let state = symbol_state(&[(LONG_EX, 1.0), (SHORT_EX, -1.0)]);
        let mut sig = signal(0.0, 1.0);

        strategy.enforce_single_leg_reduces_exposure(&mut sig, &state);

        assert_eq!(sig.short_size, 0.0);
    }

    #[test]
    fn single_leg_that_neutralizes_exposure_is_kept() {
        let strategy = strategy(config());
        // 净敞口 +3（多头多），只剩 short 腿 → 把敞口压到 +2，属收敛
        let state = symbol_state(&[(LONG_EX, 3.0)]);
        let mut sig = signal(0.0, 1.0);

        strategy.enforce_single_leg_reduces_exposure(&mut sig, &state);

        assert_eq!(sig.short_size, 1.0, "中和敞口的单腿单必须放行");
    }

    #[test]
    fn paired_legs_are_never_touched_by_single_leg_guard() {
        let strategy = strategy(config());
        let state = symbol_state(&[]);
        let mut sig = signal(1.0, 1.0);

        strategy.enforce_single_leg_reduces_exposure(&mut sig, &state);

        assert_eq!(sig.long_size, 1.0);
        assert_eq!(sig.short_size, 1.0);
    }

    // ===== pipeline 整体交互 =====

    #[test]
    fn pipeline_drops_whole_order_when_one_leg_is_blocked_from_flat() {
        let cfg = SpreadArbConfig {
            max_position_notional: 1000.0,
            ..config()
        };
        let strategy = strategy(cfg);
        // long 腿已有多头 9.9（名义 990），再买就超 1000 上限 → long 腿被拦；
        // 此时 short 腿单独成交会放大净敞口的反向暴露，必须一起作废
        let state = symbol_state(&[(LONG_EX, 9.9)]);
        let manager = state_manager(100_000.0, 0.0);
        let sig = signal(0.2, 0.2);

        let processed = strategy.process_signal(sig, &state, &manager);

        // 净敞口 +9.9，卖 0.2 → +9.7，属收敛，应放行
        assert!(processed.short_size > 0.0);
        assert_eq!(processed.long_size, 0.0);
    }

    #[test]
    fn pipeline_yields_nothing_when_equity_is_missing() {
        let strategy = strategy(config());
        let state = symbol_state(&[]);
        let manager = StateManager::new(&[SYMBOL.to_string()], 10_000);
        let sig = signal(1.0, 1.0);

        let processed = strategy.process_signal(sig, &state, &manager);

        assert_eq!(processed.long_size, 0.0);
        assert_eq!(processed.short_size, 0.0);
        assert!(strategy.make_orders(&processed).is_empty());
    }

    #[test]
    fn threshold_floor_is_ioc_slippage() {
        let cfg = config();
        let strategy = strategy(cfg.clone());
        // 杠杆远超上限 + 减仓方向 → 公式给出 0，应被 ioc_slippage 托底
        let state = symbol_state(&[(SHORT_EX, 100.0), (LONG_EX, 100.0)]);
        let manager = state_manager(1000.0, 0.0);

        let threshold =
            strategy.calculate_effective_threshold(SHORT_EX, LONG_EX, &state, &manager);

        assert!((threshold - cfg.ioc_slippage).abs() < 1e-12);
    }

    // ===== 端到端：pipeline 产出订单 =====

    #[test]
    fn emits_ioc_orders_on_both_legs_when_signal_fires() {
        let mut strategy = strategy(config());
        let mut manager = state_manager(100_000.0, 0.0);
        warm_up(&mut strategy, &mut manager, 0);

        let state = mispriced_state();
        strategy.bbo_local_ts.insert(SHORT_EX, 0);
        strategy.bbo_local_ts.insert(LONG_EX, 0);

        let sig = strategy
            .check_signal(&state, &manager, 0)
            .expect("显著错价应产生信号");
        let processed = strategy.process_signal(sig, &state, &manager);
        let orders = strategy.make_orders(&processed);

        assert_eq!(orders.len(), 2);
        let short_order = orders.iter().find(|o| o.side == Side::Short).unwrap();
        let long_order = orders.iter().find(|o| o.side == Side::Long).unwrap();
        assert_eq!(short_order.exchange, SHORT_EX);
        assert_eq!(long_order.exchange, LONG_EX);
        assert!(!short_order.reduce_only);
        // IOC 让价方向：卖单低于 bid、买单高于 ask
        match short_order.order_type {
            OrderType::Limit { price, tif } => {
                assert_eq!(tif, TimeInForce::IOC);
                assert!(price < 110.0);
            }
            _ => panic!("应为限价单"),
        }
        match long_order.order_type {
            OrderType::Limit { price, tif } => {
                assert_eq!(tif, TimeInForce::IOC);
                assert!(price > 90.0);
            }
            _ => panic!("应为限价单"),
        }
    }

    #[test]
    fn public_streams_cover_only_listed_exchanges() {
        let strategy = SpreadArbStrategy::new(
            config(),
            vec![Exchange::Binance, Exchange::OKX],
            SYMBOL.to_string(),
        );

        let streams = strategy.public_streams();

        assert_eq!(streams.len(), 2);
        assert!(streams.contains_key(&Exchange::Binance));
        assert!(streams.contains_key(&Exchange::OKX));
        assert!(!streams.contains_key(&Exchange::Hyperliquid));
    }
}

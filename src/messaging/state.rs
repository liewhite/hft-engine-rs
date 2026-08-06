use crate::domain::{Exchange, FundingRate, IndexPrice, MarkPrice, Order, OrderStatus, OrderType, Position, Side, Symbol, Timestamp, TimeInForce, BBO};
use crate::messaging::event::{ExchangeEventData, IncomeEvent};
use std::collections::HashMap;

/// 待处理订单信息（保存完整订单 + 运行时状态）
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub order: Order,
    pub status: OrderStatus,
    pub created_at: Timestamp,
}


/// 单 symbol 跨所持仓与估值汇总（观测口径）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SymbolExposure {
    /// 各交易所带符号仓位之和（净敞口，币本位）
    pub net_size: f64,
    /// Σ |仓位| × mid（总名义敞口）
    pub gross_notional: f64,
    /// Σ 仓位 × mid（净名义敞口，理想对冲下应接近 0）
    pub net_notional: f64,
    /// 相对**基线仓位**的名义变化：Σ (仓位 − 基线) × mid
    ///
    /// 基线 = 本次会话开始时的既有仓位。用于把"重启前就持有的存货"从盈亏里剔除，
    /// 否则 `cash`（本次会话从 0 起算）加上全额存货市值，会凭空多出一笔等于存货市值的假盈利。
    pub session_notional_delta: f64,
    /// 有非零仓位的交易所个数
    pub legs: usize,
    /// 有仓位但缺少 BBO、无法估值的交易所个数（估值不完整的提示）
    pub unpriced_legs: usize,
}

/// 单个交易对在所有交易所的聚合状态
#[derive(Debug, Clone)]
pub struct SymbolState {
    pub symbol: Symbol,
    pub funding_rates: HashMap<Exchange, FundingRate>,
    pub bbos: HashMap<Exchange, BBO>,
    pub mark_prices: HashMap<Exchange, MarkPrice>,
    pub index_prices: HashMap<Exchange, IndexPrice>,
    pub positions: HashMap<Exchange, Position>,
    /// 待处理订单 (以 client_order_id 为 key)
    pending_orders: HashMap<String, PendingOrder>,
}

impl SymbolState {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            funding_rates: HashMap::new(),
            bbos: HashMap::new(),
            mark_prices: HashMap::new(),
            index_prices: HashMap::new(),
            positions: HashMap::new(),
            pending_orders: HashMap::new(),
        }
    }

    /// 添加待处理订单 (发送订单信号时调用)
    pub fn add_pending_order(&mut self, order: Order, created_at: Timestamp) {
        let client_order_id = order.client_order_id.clone();
        self.pending_orders.insert(client_order_id, PendingOrder {
            order,
            status: OrderStatus::Created,
            created_at,
        });
    }

    /// 检查并移除超时订单，返回被移除的订单数量
    ///
    /// 仅清理 Created 状态超过 timeout_ms 的订单（交易所未确认，视为丢失）。
    /// 已确认的挂单（Pending/PartiallyFilled）由策略决定何时撤单，不做超时清理。
    pub fn remove_timed_out_orders(&mut self, now: Timestamp, timeout_ms: u64) -> usize {
        if timeout_ms == 0 {
            return 0;
        }
        let before = self.pending_orders.len();
        let symbol = self.symbol.clone();
        self.pending_orders.retain(|client_id, pending| {
            let elapsed = now.saturating_sub(pending.created_at);
            if elapsed > timeout_ms && pending.status == OrderStatus::Created {
                tracing::warn!(
                    symbol = %symbol,
                    client_order_id = %client_id,
                    exchange = %pending.order.exchange,
                    elapsed_ms = elapsed,
                    "Order timed out (no exchange confirmation), removing from pending"
                );
                return false;
            }
            true
        });
        before - self.pending_orders.len()
    }

    /// 获取统一时间基准（所有交易所中最近的结算时间和当前时间）
    ///
    /// 返回 (base_settle_time, current_time)
    fn unified_time_base(&self) -> Option<(Timestamp, Timestamp)> {
        if self.funding_rates.is_empty() {
            return None;
        }

        // 找出最近的 next_settle_time
        let min_settle_time = self
            .funding_rates
            .values()
            .map(|r| r.next_settle_time)
            .min()?;

        // 使用最新的 timestamp 作为当前时间
        let current_time = self
            .funding_rates
            .values()
            .map(|r| r.timestamp)
            .max()?;

        Some((min_settle_time, current_time))
    }

    /// 获取日化费率最高的交易所 (适合做空)
    ///
    /// 使用统一时间基准计算日化费率，确保跨交易所比较公平
    pub fn best_short_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        let (base_settle_time, current_time) = self.unified_time_base()?;

        self.funding_rates
            .iter()
            .max_by(|a, b| {
                let a_daily = a.1.daily_rate_with_base_time(base_settle_time, current_time);
                let b_daily = b.1.daily_rate_with_base_time(base_settle_time, current_time);
                a_daily.total_cmp(&b_daily)
            })
            .map(|(e, r)| (*e, r))
    }

    /// 获取日化费率最低的交易所 (适合做多)
    ///
    /// 使用统一时间基准计算日化费率，确保跨交易所比较公平
    pub fn best_long_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        let (base_settle_time, current_time) = self.unified_time_base()?;

        self.funding_rates
            .iter()
            .min_by(|a, b| {
                let a_daily = a.1.daily_rate_with_base_time(base_settle_time, current_time);
                let b_daily = b.1.daily_rate_with_base_time(base_settle_time, current_time);
                a_daily.total_cmp(&b_daily)
            })
            .map(|(e, r)| (*e, r))
    }

    /// 是否有持仓
    pub fn has_positions(&self) -> bool {
        self.positions.values().any(|p| !p.is_empty())
    }

    /// 获取某个交易所的仓位
    pub fn position(&self, exchange: Exchange) -> Option<&Position> {
        self.positions.get(&exchange)
    }

    /// 获取某个交易所的 BBO
    pub fn bbo(&self, exchange: Exchange) -> Option<&BBO> {
        self.bbos.get(&exchange)
    }

    /// 获取某个交易所的标记价格
    pub fn mark_price(&self, exchange: Exchange) -> Option<&MarkPrice> {
        self.mark_prices.get(&exchange)
    }

    /// 获取某个交易所的指数价格
    pub fn index_price(&self, exchange: Exchange) -> Option<&IndexPrice> {
        self.index_prices.get(&exchange)
    }

    /// 获取某个交易所的仓位大小
    ///
    /// 无仓位记录等价于空仓（size = 0.0），这是正确的业务语义：
    /// 策略启动初期确实没有仓位。
    pub fn position_size(&self, exchange: Exchange) -> f64 {
        self.positions.get(&exchange).map(|p| p.size).unwrap_or(0.0)
    }

    /// 是否有未完成订单
    pub fn has_pending_orders(&self) -> bool {
        !self.pending_orders.is_empty()
    }

    /// 是否有指定方向的未完成订单
    pub fn has_pending_side(&self, side: Side) -> bool {
        self.pending_orders.values().any(|p| p.order.side == side)
    }

    /// 获取所有待处理订单
    pub fn pending_orders(&self) -> impl Iterator<Item = &PendingOrder> {
        self.pending_orders.values()
    }

    /// 获取多空仓位大小
    ///
    /// 返回 (多头总量, 空头总量):
    /// - 多头总量: 所有正向持仓之和（正数）
    /// - 空头总量: 所有负向持仓之和（负数）
    pub fn position_sizes(&self) -> (f64, f64) {
        let mut long_size = 0.0;
        let mut short_size = 0.0;

        for pos in self.positions.values() {
            if pos.size > 0.0 {
                long_size += pos.size;
            } else if pos.size < 0.0 {
                short_size += pos.size;
            }
        }

        (long_size, short_size)
    }

    /// 更新状态
    ///
    /// 如果事件的 symbol 与 state 的 symbol 不一致，则忽略该事件
    pub fn apply(&mut self, event: &IncomeEvent) {
        // 校验 symbol 一致性 (Balance/Equity/Clock 无 symbol，直接忽略)
        if let Some(event_symbol) = event.symbol() {
            if event_symbol != &self.symbol {
                tracing::warn!(
                    expected = %self.symbol,
                    actual = %event_symbol,
                    "Event symbol mismatch, ignoring"
                );
                return;
            }
        } else {
            // Balance/Equity/Clock 无 symbol，在 per-symbol 状态中不处理
            return;
        }

        match &event.data {
            ExchangeEventData::FundingRate(rate) => {
                self.funding_rates.insert(rate.exchange, rate.clone());
            }
            ExchangeEventData::BBO(bbo) => {
                self.bbos.insert(bbo.exchange, bbo.clone());
            }
            ExchangeEventData::MarketTrade(_) => {
                // 公共成交印记仅作市场信号 (策略自取)，不修改聚合状态
            }
            ExchangeEventData::PositionBaseline(position) => {
                // 持仓维护模型：**一次性基线 + 之后全程由 Fill 事件增量维护**。
                // 基线写入一次，此后所有变化都靠 Fill 累加（见下方 Fill 分支）——主动单、
                // 手动单、以及**强平/ADL** 都以 fill 形式经私有成交流下发，因此持仓不会漏掉
                // 被动减仓。
                //
                // 为何**不**用持仓快照周期性覆写校准：快照与实时 Fill 流之间存在竞态——快照
                // 可能已包含某笔成交，而该成交对应的 Fill 稍后才由 WS 送达；若用快照覆写后又
                // 叠加这笔晚到的 Fill，就会**重复计算**该笔成交。校验走另一条通道，见
                // [`ExchangeEventData::PositionReport`] 与 `PositionReconcileActor`。
                //
                // 第二次到达是**违约**（唯一合法产地是 ManagerActor 启动期，见事件定义），
                // 打 error 而非静默忽略：静默忽略会让"某个适配层又开始发基线"这件事无人察觉。
                match self.positions.get(&position.exchange) {
                    None => {
                        tracing::info!(
                            symbol = %self.symbol,
                            exchange = %position.exchange,
                            size = position.size,
                            "Position baseline initialized"
                        );
                        self.positions.insert(position.exchange, position.clone());
                    }
                    Some(existing) => tracing::error!(
                        symbol = %self.symbol,
                        exchange = %position.exchange,
                        local_size = existing.size,
                        incoming_size = position.size,
                        "收到重复的 PositionBaseline（每个 (所, symbol) 只允许一次），已忽略"
                    ),
                }
            }
            ExchangeEventData::OrderUpdate(update) => {
                tracing::info!(
                    symbol = %self.symbol,
                    exchange = %update.exchange,
                    order_id = %update.order_id,
                    client_order_id = ?update.client_order_id,
                    status = ?update.status,
                    "Updating order status"
                );
                // 使用 client_order_id 跟踪订单状态
                // 如果没有返回 client_order_id 说明不是我们发起的订单，忽略
                if let Some(ref client_id) = update.client_order_id {
                    match update.status {
                        OrderStatus::Filled
                        | OrderStatus::Cancelled
                        | OrderStatus::Rejected { .. }
                        | OrderStatus::Error { .. } => {
                            // 订单终态，移除 pending order
                            self.pending_orders.remove(client_id);
                        }
                        OrderStatus::Pending | OrderStatus::PartiallyFilled { .. } => {
                            // 交易所已确认订单，更新状态并回填 order_id
                            if let Some(pending) = self.pending_orders.get_mut(client_id) {
                                pending.status = update.status.clone();
                                if pending.order.id.is_empty() {
                                    pending.order.id = update.order_id.clone();
                                }
                            } else if !update.exchange.owns_cli_order_id(client_id) {
                                // **不是本引擎下的单**（人工经 UI 下单、其他程序、交易所系统单）。
                                // 接管它会让它进入策略的 pending 集合，从而影响 has_pending_orders
                                // 等判断 —— 等于让别人的单左右本策略的动作。只记录，不接管。
                                tracing::info!(
                                    symbol = %self.symbol,
                                    exchange = %update.exchange,
                                    client_order_id = %client_id,
                                    "收到非本引擎挂单的更新，不纳入本地 pending"
                                );
                            } else if update.price <= 0.0 || update.quantity <= 0.0 {
                                // 本引擎的单、本地无记录，但 update 的关键字段是无效值 ——
                                // 数据不足以重建，**宁缺勿假**。真实来路：IBKR 的 sor 推送
                                // 不含价格与数量（适配层写死 0），若照建就是一张 price=0、
                                // qty=0 的幽灵挂单：has_pending_orders 从此恒真、策略比对
                                // price/qty 读到 0。缺席的代价（可能重复挂单）有界且可被
                                // 交易所拒单/风控拦住；假单的代价（symbol 永久冻结）无界。
                                tracing::warn!(
                                    symbol = %self.symbol,
                                    exchange = %update.exchange,
                                    client_order_id = %client_id,
                                    price = update.price,
                                    quantity = update.quantity,
                                    "本引擎挂单确认迟到，但更新缺有效价格/数量，不重建本地 pending"
                                );
                            } else {
                                // 本引擎的单，但本地已无记录 —— 正常只有一种来路：下单后迟迟未获
                                // 确认，被 remove_timed_out_orders 当作丢失清掉，之后确认才姗姗来迟。
                                // 这时必须重新纳入跟踪，否则它在交易所上活着、在本地却隐形，策略
                                // 会在它旁边重复挂单。
                                //
                                // （启动期交易所已存在的遗留挂单**不走这条路** —— 那些由
                                // `ManagerActor` 在启动期直接撤掉，见 `cancel_leftover_orders`。）
                                //
                                // 权威字段直接取自 update：side/price/quantity/reduce_only/status。
                                // tif 无法从订单更新可靠还原，对 resting 限价单按 GTC 占位——它不参与
                                // 后续跟踪判断（has_pending_side 看 side、gamma_scalp 看 order_type/price/qty）。
                                self.pending_orders.insert(
                                    client_id.clone(),
                                    PendingOrder {
                                        order: Order {
                                            id: update.order_id.clone(),
                                            exchange: update.exchange,
                                            symbol: update.symbol.clone(),
                                            side: update.side,
                                            order_type: OrderType::Limit {
                                                price: update.price,
                                                tif: TimeInForce::GTC,
                                            },
                                            quantity: update.quantity,
                                            reduce_only: update.reduce_only,
                                            client_order_id: client_id.clone(),
                                        },
                                        status: update.status.clone(),
                                        created_at: update.timestamp,
                                    },
                                );
                            }
                        }
                        OrderStatus::Created => {
                            // 交易所不应推送 Created（这是本地状态）。若出现说明 codec 误映射，
                            // 记录后忽略该条更新（不 panic；pending order 保持原样，等后续有效更新）。
                            tracing::error!(
                                symbol = %self.symbol,
                                exchange = %update.exchange,
                                order_id = %update.order_id,
                                "交易所推送了 Created 状态（codec 映射异常），忽略此更新"
                            );
                        }
                    }
                }
            }
            ExchangeEventData::Fill(fill) => {
                // Fill 即时更新仓位——涵盖策略单、手动单、以及强平/ADL（三者都以 fill 形式
                // 经私有成交流到达，走同一路径，见上方 PositionBaseline 分支说明）。
                //
                // 仓位/均价演进复用 Position::apply_fill（与 sim 的 Ledger 同一出处）——
                // 此前这里只累加 size，entry_price 永远停在首笔值，策略拿它算止盈/风控
                // 读到的是烂数据。已实现盈亏在本路径丢弃（策略侧不记现金账）。
                let pos = self
                    .positions
                    .entry(fill.exchange)
                    .or_insert_with(|| Position::empty(fill.exchange, self.symbol.clone()));
                let _realized = pos.apply_fill(fill.side, fill.price, fill.size);
                // 浮盈以最新成交价近似刷新（本路径没有独立行情输入；基线/读数路径的
                // unrealized_pnl 是交易所计算值，两种口径都比"永远停在启动快照"诚实）
                pos.unrealized_pnl = (fill.price - pos.entry_price) * pos.size;
                tracing::info!(
                    symbol = %self.symbol,
                    exchange = %fill.exchange,
                    side = ?fill.side,
                    fill_size = fill.size,
                    fill_price = fill.price,
                    reason = ?fill.reason,
                    new_position_size = pos.size,
                    entry_price = pos.entry_price,
                    "Updated position on fill"
                );
            }
            ExchangeEventData::MarkPrice(mp) => {
                self.mark_prices.insert(mp.exchange, mp.clone());
            }
            ExchangeEventData::IndexPrice(ip) => {
                self.index_prices.insert(ip.exchange, ip.clone());
            }
            ExchangeEventData::Candle(_) | ExchangeEventData::HistoryCandles(_) => {
                // K线数据由策略层处理，SymbolState 不存储
            }
            ExchangeEventData::FundingFee(_) => {
                // 资费事件不修改本地 symbol 状态，下游策略自行去重与统计
            }
            ExchangeEventData::Clock => {
                // Clock 事件由策略层处理，这里不需要处理
            }
            ExchangeEventData::Greeks(_)
            | ExchangeEventData::Balance(_)
            | ExchangeEventData::AccountInfo { .. }
            | ExchangeEventData::ExchangeStatus { .. }
            | ExchangeEventData::BorrowFee(_)
            | ExchangeEventData::ExchangeRate(_)
            // 自定义事件不进状态层：框架只运送不理解（见 CustomEvent）
            | ExchangeEventData::Custom(_)
            // 对账读数是按所的整份快照（无单一 symbol），且**绝不写入持仓** ——
            // 写进来就等于恢复了上面 PositionBaseline 分支否掉的"快照覆写"
            | ExchangeEventData::PositionReport { .. } => {
                // 全局事件应在 StateManager 层提前拦截、不会进入 SymbolState::apply。
                // 若到达说明路由逻辑有 bug，记录后忽略（不 panic）。
                tracing::error!(
                    symbol = %self.symbol,
                    "全局事件错误地进入 SymbolState::apply（路由 bug），忽略"
                );
            }
        }
    }

    /// 移除指定的待处理订单
    pub fn remove_pending_order(&mut self, client_order_id: &str) {
        self.pending_orders.remove(client_order_id);
    }

    /// 汇总跨所持仓与估值
    ///
    /// `baseline` 为各所的会话起始仓位（缺失视为 0）。缺 BBO 的腿不参与估值但计入
    /// `unpriced_legs`——宁可显式暴露"估值不完整"，也不用兜底价格伪造一个看似完整的数字。
    pub fn exposure(&self, baseline: Option<&HashMap<Exchange, f64>>) -> SymbolExposure {
        let mut exposure = SymbolExposure::default();
        for (exchange, position) in &self.positions {
            if position.is_empty() {
                continue;
            }
            exposure.legs += 1;
            exposure.net_size += position.size;
            match self.bbo(*exchange) {
                Some(bbo) => {
                    let mid = bbo.mid_price();
                    let base = baseline
                        .and_then(|b| b.get(exchange))
                        .copied()
                        .unwrap_or(0.0);
                    exposure.gross_notional += position.size.abs() * mid;
                    exposure.net_notional += position.size * mid;
                    exposure.session_notional_delta += (position.size - base) * mid;
                }
                None => exposure.unpriced_legs += 1,
            }
        }
        exposure
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYMBOL: &str = "BTC";

    fn bbo(exchange: Exchange, mid: f64) -> BBO {
        BBO {
            exchange,
            symbol: SYMBOL.to_string(),
            bid_price: mid,
            bid_qty: 1.0,
            ask_price: mid,
            ask_qty: 1.0,
            timestamp: 0,
        }
    }

    fn state_with(positions: &[(Exchange, f64)], priced: &[(Exchange, f64)]) -> SymbolState {
        let mut state = SymbolState::new(SYMBOL.to_string());
        for &(exchange, size) in positions {
            state.positions.insert(
                exchange,
                Position {
                    exchange,
                    symbol: SYMBOL.to_string(),
                    size,
                    entry_price: 100.0,
                    unrealized_pnl: 0.0,
                },
            );
        }
        for &(exchange, mid) in priced {
            state.bbos.insert(exchange, bbo(exchange, mid));
        }
        state
    }

    #[test]
    fn hedged_position_has_near_zero_net_notional() {
        let state = state_with(
            &[(Exchange::Binance, 1.0), (Exchange::OKX, -1.0)],
            &[(Exchange::Binance, 100.0), (Exchange::OKX, 100.0)],
        );

        let exposure = state.exposure(None);

        assert_eq!(exposure.legs, 2);
        assert!(exposure.net_notional.abs() < 1e-9);
        assert!((exposure.gross_notional - 200.0).abs() < 1e-9);
        assert_eq!(exposure.unpriced_legs, 0);
    }

    #[test]
    fn baseline_position_is_excluded_from_session_delta() {
        let state = state_with(&[(Exchange::Binance, 3.0)], &[(Exchange::Binance, 100.0)]);
        let baseline = HashMap::from([(Exchange::Binance, 2.0)]);

        let exposure = state.exposure(Some(&baseline));

        // 全额估值 300，但本次会话只新增了 1 个 → 100
        assert!((exposure.net_notional - 300.0).abs() < 1e-9);
        assert!((exposure.session_notional_delta - 100.0).abs() < 1e-9);
    }

    #[test]
    fn missing_baseline_defaults_to_zero() {
        let state = state_with(&[(Exchange::Binance, 3.0)], &[(Exchange::Binance, 100.0)]);
        let exposure = state.exposure(Some(&HashMap::new()));
        assert!((exposure.session_notional_delta - 300.0).abs() < 1e-9);
    }

    #[test]
    fn missing_bbo_is_surfaced_not_silently_valued_at_zero() {
        let state = state_with(&[(Exchange::Binance, 1.0)], &[]);

        let exposure = state.exposure(None);

        assert_eq!(exposure.legs, 1);
        assert_eq!(exposure.unpriced_legs, 1);
        assert_eq!(exposure.gross_notional, 0.0);
    }

    #[test]
    fn zero_positions_are_ignored() {
        let state = state_with(&[(Exchange::Binance, 0.0)], &[(Exchange::Binance, 100.0)]);
        assert_eq!(state.exposure(None), SymbolExposure::default());
    }

    // ===== 持仓维护模型：基线只来一次 + Fill 累加 + 对账读数不写入 =====

    const EX: Exchange = Exchange::Binance;

    fn ev(data: ExchangeEventData) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: 1,
            local_ts: 1,
            data,
        }
    }

    fn position(size: f64) -> Position {
        Position {
            exchange: EX,
            symbol: SYMBOL.to_string(),
            size,
            entry_price: 100.0,
            unrealized_pnl: 0.0,
        }
    }

    fn fill(side: Side, size: f64) -> IncomeEvent {
        ev(ExchangeEventData::Fill(crate::domain::Fill {
            exchange: EX,
            symbol: SYMBOL.to_string(),
            side,
            price: 100.0,
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: 1,
            fee: 0.0,
            reason: crate::domain::FillReason::Normal,
        }))
    }

    /// 基线写入一次，之后**只**由 Fill 累加。
    #[test]
    fn baseline_initializes_then_fills_accumulate() {
        let mut state = SymbolState::new(SYMBOL.to_string());

        state.apply(&ev(ExchangeEventData::PositionBaseline(position(2.0))));
        assert_eq!(state.position_size(EX), 2.0);

        state.apply(&fill(Side::Long, 1.0));
        assert_eq!(state.position_size(EX), 3.0);

        state.apply(&fill(Side::Short, 0.5));
        assert!((state.position_size(EX) - 2.5).abs() < 1e-12);
    }

    /// **Critical 回归防线**：第二条基线不得覆写持仓。
    ///
    /// 快照覆写 + 晚到的 Fill 会重复计算同一笔成交。历史上 OKX/HL/Binance 的私有 WS 都在
    /// 持续推送持仓快照，全靠这条判据兜住；现在适配层已不再发基线，这条测试是防止它们
    /// 哪天又开始发的防线。
    #[test]
    fn second_baseline_never_overwrites_position() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        state.apply(&ev(ExchangeEventData::PositionBaseline(position(2.0))));
        state.apply(&fill(Side::Long, 1.0));
        assert_eq!(state.position_size(EX), 3.0);

        // 交易所侧的快照（哪怕数值"看起来更新"）也不得覆写
        state.apply(&ev(ExchangeEventData::PositionBaseline(position(3.0))));
        assert_eq!(
            state.position_size(EX),
            3.0,
            "第二条基线覆写了持仓——快照与 Fill 流叠加会重复计算成交"
        );

        // 覆写若发生，这条 Fill 会把仓位推到 4.0 之外
        state.apply(&fill(Side::Long, 1.0));
        assert_eq!(state.position_size(EX), 4.0);
    }

    // ===== 外部挂单：只接管本引擎自己的单 =====

    fn order_update(client_order_id: Option<String>, status: OrderStatus) -> IncomeEvent {
        ev(ExchangeEventData::OrderUpdate(crate::domain::OrderUpdate {
            order_id: "ex-1".to_string(),
            client_order_id,
            exchange: EX,
            symbol: SYMBOL.to_string(),
            side: Side::Long,
            status,
            price: 100.0,
            reduce_only: false,
            quantity: 1.0,
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: 1,
        }))
    }

    /// 本引擎的单在本地无记录时要重新纳入跟踪。
    ///
    /// 来路：下单后迟迟未获确认、被超时清理，之后确认姗姗来迟。若不重新纳入，它在交易所上
    /// 活着、在本地却隐形，策略会在旁边重复挂单。
    #[test]
    fn own_order_missing_locally_is_re_registered() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        let own_id = EX.new_cli_order_id();

        state.apply(&order_update(Some(own_id.clone()), OrderStatus::Pending));

        let tracked: Vec<String> = state
            .pending_orders()
            .map(|p| p.order.client_order_id.clone())
            .collect();
        assert_eq!(tracked, vec![own_id]);
    }

    /// **不接管别人的单**：人工经 UI 下单 / 其他程序 / 交易所系统单。
    ///
    /// 接管会让它进入策略的 pending 集合，从而左右 has_pending_orders 等判断 ——
    /// 等于让别人的挂单决定本策略的动作。
    #[test]
    fn foreign_order_is_not_adopted_into_pending() {
        let mut state = SymbolState::new(SYMBOL.to_string());

        // Binance 网页/App 下单的典型 client_order_id 形态
        for foreign in ["web_1a2b3c", "android_9f8e", "x-someone-else"] {
            state.apply(&order_update(
                Some(foreign.to_string()),
                OrderStatus::Pending,
            ));
        }
        assert!(
            !state.has_pending_orders(),
            "外部挂单被接管进了本地 pending：{:?}",
            state.pending_orders().map(|p| &p.order.client_order_id).collect::<Vec<_>>()
        );
    }

    /// 没有基线时，第一条 Fill 从 0 起算（策略启动初期确实空仓）
    #[test]
    fn fill_without_baseline_starts_from_zero() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        state.apply(&fill(Side::Short, 1.5));
        assert_eq!(state.position_size(EX), -1.5);
    }

    /// **entry_price 随 Fill 维护加权均价**（与 sim 的 Ledger 同一逻辑）。
    /// 此前只累加 size、均价永远停在首笔值 —— 拿它算止盈价的策略读到的是烂数据。
    #[test]
    fn entry_price_tracks_weighted_average_across_fills() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        // 基线：2.0 @ 100（交易所均价）
        state.apply(&ev(ExchangeEventData::PositionBaseline(position(2.0))));

        // 同向加仓 2.0 @ 110 -> 均价 (2*100 + 2*110)/4 = 105
        let mut add = fill(Side::Long, 2.0);
        if let ExchangeEventData::Fill(f) = &mut add.data {
            f.price = 110.0;
        }
        state.apply(&add);
        let pos = state.position(EX).expect("position");
        assert!((pos.size - 4.0).abs() < 1e-12);
        assert!((pos.entry_price - 105.0).abs() < 1e-12, "均价应为 105, got {}", pos.entry_price);

        // 部分平仓 1.0 @ 120 -> 均价不变
        let mut close = fill(Side::Short, 1.0);
        if let ExchangeEventData::Fill(f) = &mut close.data {
            f.price = 120.0;
        }
        state.apply(&close);
        let pos = state.position(EX).expect("position");
        assert!((pos.size - 3.0).abs() < 1e-12);
        assert!((pos.entry_price - 105.0).abs() < 1e-12, "部分平仓不该动均价");
    }

    /// **宁缺勿假**：update 缺有效价格/数量（IBKR sor 推送写死 0）时不重建本地 pending。
    ///
    /// 照建就是一张 price=0、qty=0 的幽灵挂单：has_pending_orders 从此恒真、
    /// 策略比对 price/qty 读到 0 —— symbol 被永久冻结。
    #[test]
    fn re_registration_requires_valid_price_and_quantity() {
        let mut state = SymbolState::new(SYMBOL.to_string());
        let own_id = EX.new_cli_order_id();

        let mut ghost = order_update(Some(own_id), OrderStatus::Pending);
        if let ExchangeEventData::OrderUpdate(u) = &mut ghost.data {
            u.price = 0.0;
            u.quantity = 0.0;
        }
        state.apply(&ghost);

        assert!(
            !state.has_pending_orders(),
            "price/qty 为 0 的更新被重建成了幽灵挂单"
        );
    }
}

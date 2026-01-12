use crate::domain::{Exchange, FundingRate, IndexPrice, MarkPrice, OrderStatus, Position, Side, Symbol, Timestamp, BBO};
use crate::messaging::event::{ExchangeEventData, IncomeEvent};
use std::collections::HashMap;

/// 待处理订单信息
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub exchange: Exchange,
    pub side: Side,
    pub quantity: f64,
    pub status: OrderStatus,
    pub created_at: Timestamp,
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
    pub fn add_pending_order(
        &mut self,
        client_order_id: String,
        exchange: Exchange,
        side: Side,
        quantity: f64,
        created_at: Timestamp,
    ) {
        self.pending_orders.insert(client_order_id, PendingOrder {
            exchange,
            side,
            quantity,
            status: OrderStatus::Created,
            created_at,
        });
    }

    /// 检查并移除超时订单，返回被移除的订单数量
    ///
    /// 只移除 Created 状态的订单（交易所未确认）
    pub fn remove_timed_out_orders(&mut self, now: Timestamp, timeout_ms: u64) -> usize {
        let before = self.pending_orders.len();
        self.pending_orders.retain(|client_id, order| {
            let elapsed = now.saturating_sub(order.created_at);
            if elapsed > timeout_ms && order.status == OrderStatus::Created {
                tracing::warn!(
                    client_order_id = %client_id,
                    exchange = %order.exchange,
                    elapsed_ms = elapsed,
                    "Order timed out (no exchange confirmation), removing from pending"
                );
                false
            } else {
                true
            }
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

    /// 是否有未完成订单
    pub fn has_pending_orders(&self) -> bool {
        !self.pending_orders.is_empty()
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

    /// 计算净敞口 (net exposure)
    ///
    /// 返回 (净持仓量, 估算价格):
    /// - 净持仓量: 正数表示净多头，负数表示净空头，0 表示完全对冲或无持仓
    /// - 估算价格: 用于计算敞口价值
    pub fn net_exposure(&self) -> (f64, f64) {
        let positions: Vec<_> = self.positions.values()
            .filter(|p| !p.is_empty())
            .collect();

        if positions.is_empty() {
            return (0.0, 0.0);
        }

        // 计算净持仓 (size 本身带符号：正数多头，负数空头)
        let net_position: f64 = positions.iter().map(|p| p.size).sum();

        // 估算价格：使用 entry_price
        let price = positions
            .iter()
            .find_map(|p| {
                if p.entry_price > 0.0 {
                    Some(p.entry_price)
                } else {
                    None
                }
            })
            .unwrap_or(0.0);

        (net_position, price)
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
            ExchangeEventData::Position(position) => {
                // 只在没有 pending orders 时更新，避免覆盖乐观更新
                if self.pending_orders.is_empty() {
                    self.positions.insert(position.exchange, position.clone());
                }
            }
            ExchangeEventData::OrderUpdate(update) => {
                tracing::info!(
                    exchange = %update.exchange,
                    order_id = %update.order_id,
                    client_order_id = ?update.client_order_id,
                    status = ?update.status,
                    "Updating order status"
                );
                // 使用 client_order_id 跟踪订单状态
                // 如果没有返回client_order_id说明不是我们发起的订单，忽略
                if let Some(ref client_id) = update.client_order_id {
                    match update.status {
                        OrderStatus::Filled => {
                            // 订单成交，乐观更新仓位并移除
                            if let Some(order) = self.pending_orders.remove(client_id) {
                                if let Some(pos) = self.positions.get_mut(&order.exchange) {
                                    let delta = match order.side {
                                        Side::Long => order.quantity,
                                        Side::Short => -order.quantity,
                                    };
                                    pos.size += delta;
                                    tracing::info!(
                                        exchange = %order.exchange,
                                        side = ?order.side,
                                        quantity = order.quantity,
                                        new_size = pos.size,
                                        "Optimistically updated position on order filled"
                                    );
                                }
                            }
                        }
                        OrderStatus::Cancelled
                        | OrderStatus::Rejected { .. }
                        | OrderStatus::Error { .. } => {
                            // 订单取消或失败，仅移除
                            self.pending_orders.remove(client_id);
                        }
                        OrderStatus::Pending | OrderStatus::PartiallyFilled { .. } => {
                            // 交易所已确认订单，更新状态
                            if let Some(order) = self.pending_orders.get_mut(client_id) {
                                order.status = update.status.clone();
                            }
                        }
                        OrderStatus::Created => {
                            // 交易所不会推送 Created 状态，这是本地状态
                            // 如果收到说明有 bug
                            unreachable!("Exchange should never push Created status")
                        }
                    }
                }
            }
            ExchangeEventData::MarkPrice(mp) => {
                self.mark_prices.insert(mp.exchange, mp.clone());
            }
            ExchangeEventData::IndexPrice(ip) => {
                self.index_prices.insert(ip.exchange, ip.clone());
            }
            ExchangeEventData::Clock => {
                // Clock 事件由策略层处理，这里不需要处理
            }
            ExchangeEventData::Balance(_) | ExchangeEventData::AccountInfo { .. } => {
                // 已在上面提前返回，这里不会执行
                unreachable!()
            }
        }
    }

    /// 移除指定的待处理订单
    pub fn remove_pending_order(&mut self, client_order_id: &str) {
        self.pending_orders.remove(client_order_id);
    }
}

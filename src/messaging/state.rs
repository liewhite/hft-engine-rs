use crate::domain::{Exchange, FundingRate, OrderStatus, Position, Rate, Symbol, Timestamp, BBO};

/// 复用 Position 的 epsilon 常量
const POSITION_EPSILON: f64 = Position::EPSILON;
use crate::messaging::event::ExchangeEvent;
use std::collections::HashMap;

/// 待处理订单状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOrderStatus {
    /// 已创建信号，等待交易所确认
    Created,
    /// 交易所已确认
    Confirmed,
}

/// 待处理订单信息
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub exchange: Exchange,
    pub status: PendingOrderStatus,
    pub created_at: Timestamp,
}


/// 单个交易对在所有交易所的聚合状态
#[derive(Debug, Clone)]
pub struct SymbolState {
    pub symbol: Symbol,
    pub funding_rates: HashMap<Exchange, FundingRate>,
    pub bbos: HashMap<Exchange, BBO>,
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
            positions: HashMap::new(),
            pending_orders: HashMap::new(),
        }
    }

    /// 添加待处理订单 (发送订单信号时调用)
    pub fn add_pending_order(&mut self, client_order_id: String, exchange: Exchange, created_at: Timestamp) {
        self.pending_orders.insert(client_order_id, PendingOrder {
            exchange,
            status: PendingOrderStatus::Created,
            created_at,
        });
    }

    /// 检查并移除超时订单，返回被移除的订单数量
    pub fn remove_timed_out_orders(&mut self, now: Timestamp, timeout_ms: u64) -> usize {
        let before = self.pending_orders.len();
        self.pending_orders.retain(|client_id, order| {
            let elapsed = now.saturating_sub(order.created_at);
            if elapsed > timeout_ms && order.status == PendingOrderStatus::Created {
                tracing::warn!(
                    client_order_id = %client_id,
                    exchange = %order.exchange,
                    elapsed_ms = elapsed,
                    "Order timed out, removing from pending"
                );
                false
            } else {
                true
            }
        });
        before - self.pending_orders.len()
    }

    /// 获取日化费率最高的交易所 (适合做空)
    pub fn best_short_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        self.funding_rates
            .iter()
            .max_by(|a, b| a.1.daily_rate().total_cmp(&b.1.daily_rate()))
            .map(|(e, r)| (*e, r))
    }

    /// 获取日化费率最低的交易所 (适合做多)
    pub fn best_long_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        self.funding_rates
            .iter()
            .min_by(|a, b| a.1.daily_rate().total_cmp(&b.1.daily_rate()))
            .map(|(e, r)| (*e, r))
    }

    /// 计算日化费率差
    pub fn funding_spread(&self) -> Option<Rate> {
        let (_, high) = self.best_short_exchange()?;
        let (_, low) = self.best_long_exchange()?;
        Some(high.daily_rate() - low.daily_rate())
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

    /// 是否有未完成订单
    pub fn has_pending_orders(&self) -> bool {
        !self.pending_orders.is_empty()
    }

    /// 检查持仓是否对冲 (多空持仓量是否匹配)
    /// 返回 None 表示无持仓，Some(true) 表示对冲，Some(false) 表示存在敞口
    pub fn is_hedged(&self) -> Option<bool> {
        let positions: Vec<_> = self.positions.values()
            .filter(|p| !p.is_empty())
            .collect();

        if positions.is_empty() {
            return None;
        }

        // 计算净持仓 (size 本身带符号：正数多头，负数空头)
        let net_position: f64 = positions.iter().map(|p| p.size).sum();

        // 净持仓接近 0 视为对冲
        Some(net_position.abs() < POSITION_EPSILON)
    }

    /// 获取不平衡的敞口持仓 (返回需要平仓的交易所、方向和数量)
    ///
    /// 计算净持仓，找出需要平仓的交易所和方向：
    /// - 如果净持仓为正（净多头），在持仓为正的交易所平多头
    /// - 如果净持仓为负（净空头），在持仓为负的交易所平空头
    pub fn unhedged_exposure(&self) -> Option<(Exchange, crate::domain::Side, f64)> {
        use crate::domain::Side;

        let positions: Vec<_> = self.positions.iter()
            .filter(|(_, p)| !p.is_empty())
            .collect();

        if positions.is_empty() {
            return None;
        }

        // 单腿持仓直接返回
        if positions.len() == 1 {
            let (ex, pos) = positions[0];
            return Some((*ex, pos.side(), pos.size.abs()));
        }

        // 计算净持仓 (size 带符号)
        let net_position: f64 = positions.iter().map(|(_, p)| p.size).sum();

        if net_position.abs() < POSITION_EPSILON {
            return None; // 已对冲
        }

        // 找出需要平仓的交易所：
        // - 净多头 (net > 0)：在持仓为正的交易所平多头
        // - 净空头 (net < 0)：在持仓为负的交易所平空头
        let (exchange, side) = if net_position > 0.0 {
            // 找持仓最大的多头交易所
            positions.iter()
                .filter(|(_, p)| p.size > 0.0)
                .max_by(|a, b| a.1.size.total_cmp(&b.1.size))
                .map(|(ex, _)| (*ex, Side::Long))?
        } else {
            // 找持仓最大的空头交易所
            positions.iter()
                .filter(|(_, p)| p.size < 0.0)
                .min_by(|a, b| a.1.size.total_cmp(&b.1.size))
                .map(|(ex, _)| (*ex, Side::Short))?
        };

        Some((*exchange, side, net_position.abs()))
    }

    /// 更新状态
    ///
    /// 如果事件的 symbol 与 state 的 symbol 不一致，则忽略该事件
    pub fn apply(&mut self, event: &ExchangeEvent) {
        // 校验 symbol 一致性 (BalanceUpdate 无 symbol，直接忽略)
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
            // BalanceUpdate 无 symbol，在 per-symbol 状态中不处理
            return;
        }

        match event {
            ExchangeEvent::FundingRateUpdate { exchange, rate, .. } => {
                self.funding_rates.insert(*exchange, rate.clone());
            }
            ExchangeEvent::BBOUpdate { exchange, bbo, .. } => {
                self.bbos.insert(*exchange, bbo.clone());
            }
            ExchangeEvent::PositionUpdate {
                exchange, position, ..
            } => {
                tracing::info!(
                    exchange = %exchange,
                    symbol = %self.symbol,
                    size = position.size,
                    "Updating position"
                );
                self.positions.insert(*exchange, position.clone());
            }
            ExchangeEvent::OrderStatusUpdate {
                exchange, update, ..
            } => {
                tracing::info!(
                    exchange = %exchange,
                    order_id = %update.order_id,
                    client_order_id = ?update.client_order_id,
                    status = ?update.status,
                    "Updating order status"
                );
                // 使用 client_order_id 跟踪订单状态
                if let Some(ref client_id) = update.client_order_id {
                    match update.status {
                        OrderStatus::Filled
                        | OrderStatus::Cancelled
                        | OrderStatus::Rejected { .. }
                        | OrderStatus::Error { .. } => {
                            // 订单结束或失败，移除
                            self.pending_orders.remove(client_id);
                        }
                        OrderStatus::Pending | OrderStatus::PartiallyFilled { .. } => {
                            // 交易所已确认订单，更新状态
                            if let Some(order) = self.pending_orders.get_mut(client_id) {
                                order.status = PendingOrderStatus::Confirmed;
                            }
                        }
                    }
                }
            }
            ExchangeEvent::Clock { .. } => {
                // Clock 事件由策略层处理，这里不需要处理
            }
            ExchangeEvent::BalanceUpdate { .. } | ExchangeEvent::EquityUpdate { .. } => {
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

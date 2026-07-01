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
            ExchangeEventData::Position(position) => {
                // 持仓维护模型：**一次性初始化 + 之后全程由 Fill 事件增量维护**。
                // 本地无该交易所持仓时用快照初始化一次，此后所有变化都靠 Fill 累加
                // （见下方 Fill 分支）——主动单、手动单、以及**强平/ADL** 都以 fill 形式经
                // 私有成交流下发，因此持仓不会漏掉被动减仓。
                //
                // 为何**不**用 Position 快照周期性"对账校准"：REST/WS 拉取的持仓快照与实时
                // Fill 流之间存在竞态——快照可能已包含某笔成交，而该成交对应的 Fill 稍后才
                // 由 WS 送达；若用快照覆写后又叠加这笔晚到的 Fill，就会**重复计算**该笔成交。
                // 故快照只做首次初始化，绝不在运行期覆写。
                if !self.positions.contains_key(&position.exchange) {
                    tracing::info!(
                        symbol = %self.symbol,
                        exchange = %position.exchange,
                        size = position.size,
                        "Position initialized from poll"
                    );
                    self.positions.insert(position.exchange, position.clone());
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
                            } else {
                                // 启动时同步的现有挂单，注册到 pending_orders
                                // 外部挂单（启动/重连时交易所已存在、本地无记录）重建为 PendingOrder。
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
                // 经私有成交流到达，走同一路径，无需快照对账，见上方 Position 分支说明）。
                let delta = match fill.side {
                    Side::Long => fill.size,
                    Side::Short => -fill.size,
                };
                let pos = self.positions.entry(fill.exchange).or_insert_with(|| {
                    Position {
                        exchange: fill.exchange,
                        symbol: self.symbol.clone(),
                        size: 0.0,
                        entry_price: fill.price,
                        unrealized_pnl: 0.0,
                    }
                });
                pos.size += delta;
                tracing::info!(
                    symbol = %self.symbol,
                    exchange = %fill.exchange,
                    side = ?fill.side,
                    fill_size = fill.size,
                    fill_price = fill.price,
                    reason = ?fill.reason,
                    new_position_size = pos.size,
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
            | ExchangeEventData::ExchangeStatus { .. } => {
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
}

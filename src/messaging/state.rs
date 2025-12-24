use crate::domain::{Exchange, FundingRate, OrderId, OrderStatus, Position, Rate, Symbol, BBO};
use crate::messaging::event::ExchangeEvent;
use std::collections::{HashMap, HashSet};

/// 单个交易对在所有交易所的聚合状态
#[derive(Debug, Clone)]
pub struct SymbolState {
    pub symbol: Symbol,
    pub funding_rates: HashMap<Exchange, FundingRate>,
    pub bbos: HashMap<Exchange, BBO>,
    pub positions: HashMap<Exchange, Position>,
    pub pending_orders: HashSet<(Exchange, OrderId)>,
}

impl SymbolState {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            funding_rates: HashMap::new(),
            bbos: HashMap::new(),
            positions: HashMap::new(),
            pending_orders: HashSet::new(),
        }
    }

    /// 获取费率最高的交易所 (适合做空)
    pub fn best_short_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        self.funding_rates
            .iter()
            .max_by(|a, b| a.1.rate.total_cmp(&b.1.rate))
            .map(|(e, r)| (*e, r))
    }

    /// 获取费率最低的交易所 (适合做多)
    pub fn best_long_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        self.funding_rates
            .iter()
            .min_by(|a, b| a.1.rate.total_cmp(&b.1.rate))
            .map(|(e, r)| (*e, r))
    }

    /// 计算费率差
    pub fn funding_spread(&self) -> Option<Rate> {
        let (_, high) = self.best_short_exchange()?;
        let (_, low) = self.best_long_exchange()?;
        Some(high.rate - low.rate)
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

        // 计算净持仓 (多头为正，空头为负)
        let net_position: f64 = positions.iter()
            .map(|p| match p.side {
                crate::domain::Side::Long => p.size,
                crate::domain::Side::Short => -p.size,
            })
            .sum();

        // 净持仓接近 0 视为对冲
        Some(net_position.abs() < 1e-10)
    }

    /// 获取不平衡的敞口持仓 (返回需要平仓的交易所和数量)
    pub fn unhedged_exposure(&self) -> Option<(Exchange, crate::domain::Side, f64)> {
        let positions: Vec<_> = self.positions.iter()
            .filter(|(_, p)| !p.is_empty())
            .collect();

        if positions.len() != 2 {
            // 非双腿持仓暂不处理
            if let Some((ex, pos)) = positions.first() {
                return Some((**ex, pos.side, pos.size));
            }
            return None;
        }

        let (ex1, pos1) = positions[0];
        let (ex2, pos2) = positions[1];

        // 计算两边持仓差
        let diff = (pos1.size - pos2.size).abs();
        if diff < 1e-10 {
            return None; // 已对冲
        }

        // 返回持仓较大一方的超额部分
        if pos1.size > pos2.size {
            Some((*ex1, pos1.side, diff))
        } else {
            Some((*ex2, pos2.side, diff))
        }
    }

    /// 更新状态
    pub fn apply(&mut self, event: ExchangeEvent) {
        match event {
            ExchangeEvent::FundingRateUpdate { exchange, rate, .. } => {
                self.funding_rates.insert(exchange, rate);
            }
            ExchangeEvent::BBOUpdate { exchange, bbo, .. } => {
                self.bbos.insert(exchange, bbo);
            }
            ExchangeEvent::PositionUpdate {
                exchange, position, ..
            } => {
                self.positions.insert(exchange, position);
            }
            ExchangeEvent::OrderStatusUpdate {
                exchange, update, ..
            } => {
                let key = (exchange, update.order_id.clone());
                match update.status {
                    OrderStatus::Filled
                    | OrderStatus::Cancelled
                    | OrderStatus::Rejected { .. } => {
                        self.pending_orders.remove(&key);
                    }
                    OrderStatus::Pending | OrderStatus::PartiallyFilled { .. } => {
                        self.pending_orders.insert(key);
                    }
                }
            }
            ExchangeEvent::BalanceUpdate { .. } => {
                // Balance 在全局追踪，不在 per-symbol 状态中
            }
        }
    }
}

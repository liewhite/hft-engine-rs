//! Gamma scalping 策略 (long-gamma 的 maker 对冲)。移植自 ox-demo `hft.strategy.GammaScalpStrategy`。
//!
//! **trade-native，不依赖盘口**：以最新真实成交价为基准挂单。持有期权 (希腊字母经 GreeksUpdate
//! 推送：实盘 OKX 轮询 / 回测 BS 合成)，在永续上 **PostOnly 挂单**把账户净 delta 维持在对称容忍带
//! [-deltaBand, +deltaBand] 内：
//!   - 净 delta > band (偏多)：最新价上方挂卖单，价格继续上行 (真实成交越价) 时成交 (卖高)；
//!   - 净 delta < -band (偏空)：最新价下方挂买单，价格继续下行时成交 (买低)。
//!
//! 即 long-gamma 的 maker 对冲：价格波动驱动期权 delta 漂移、越带触发逐次 rehedge，maker 成交
//! 赚取波动 (毛 gamma 收益)，成本为手续费 (期权 theta/权利金由回测侧 BS 估值单独计入期权腿)。
//!
//! 净 delta = 期权 delta (含现货 cashBal 修正) + 永续持仓。同一时刻最多一张在途对冲单，挂单
//! **不追价** (静置等成交以兑现固定对冲间距)，仅方向翻转/数量显著漂移/回带内时撤单。
//!
//! **注**：ox-demo 的 MACD 方向性间距偏移 (KlineSeries/MACD overlay) 暂未移植，本实现为**对称对冲**
//! (买卖间距同为 `base_offset_ratio`)，是 gamma scalp 的最小核心形态。

use crate::domain::{Exchange, Order, OrderId, OrderType, Price, Quantity, Side, Symbol, TimeInForce};
use crate::exchange::SubscriptionKind;
use crate::messaging::{AccountData, IncomeEvent, MarketData, PendingOrder};
use crate::strategy::{OutcomeEvent, Strategy, StrategyView};
use std::collections::{HashMap, HashSet};

/// 对冲数量相对目标漂移超该比例则撤单重挂 (默认值)
const DEFAULT_QTY_TOLERANCE_RATIO: f64 = 0.2;
/// 最小对冲数量 (币本位)，低于此不挂单避免碎单 (默认值)
const DEFAULT_MIN_HEDGE_QTY: f64 = 0.001;

pub struct GammaScalpStrategy {
    /// 永续对冲场所 + greeks 标记的交易所 (回测中二者一致)
    exchange: Exchange,
    /// 永续 symbol, e.g. "ETHUSDT"
    symbol: Symbol,
    /// greeks 币种, e.g. "ETH"
    ccy: String,
    /// 净 delta 对称容忍带 (币本位)：|净 delta| 超过即对冲回中性
    delta_band: f64,
    /// 对冲间距 (PostOnly 挂单距最新成交价的偏移)，0.002 = 0.2%
    base_offset_ratio: f64,
    /// 对冲数量相对目标漂移超该比例则撤单重挂
    qty_tolerance_ratio: f64,
    /// 最小对冲数量 (币本位)，低于此不挂单避免碎单
    min_hedge_qty: Quantity,

    /// 已发出撤单、尚未确认移除的订单，防止重复撤单
    cancelling: HashSet<OrderId>,
    /// 最新真实成交价 (greeks 触发对冲时作基准)
    last_price: Option<Price>,
}

impl GammaScalpStrategy {
    pub fn new(
        exchange: Exchange,
        symbol: Symbol,
        ccy: String,
        delta_band: f64,
        base_offset_ratio: f64,
    ) -> Self {
        Self {
            exchange,
            symbol,
            ccy,
            delta_band,
            base_offset_ratio,
            qty_tolerance_ratio: DEFAULT_QTY_TOLERANCE_RATIO,
            min_hedge_qty: DEFAULT_MIN_HEDGE_QTY,
            cancelling: HashSet::new(),
            last_price: None,
        }
    }

    /// 越带的目标对冲：把净 delta 拉回中性。偏多 -> 上方挂卖；偏空 -> 下方挂买。
    /// 返回 (方向, 数量, 挂单价)；带内返回 None。
    fn desired_hedge(&self, ref_price: Price, net_delta: f64) -> Option<(Side, Quantity, Price)> {
        if net_delta > self.delta_band {
            Some((Side::Short, net_delta, ref_price * (1.0 + self.base_offset_ratio)))
        } else if net_delta < -self.delta_band {
            Some((Side::Long, -net_delta, ref_price * (1.0 - self.base_offset_ratio)))
        } else {
            None
        }
    }

    fn hedge(&mut self, ref_price: Price, view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        let Some(symbol_state) = view.symbol(&self.symbol) else {
            return Vec::new();
        };
        // greeks 与 cashBal 均到达才动作 (delta 修正就绪)
        let Some(greeks) = view.greeks(self.exchange, &self.ccy) else {
            return Vec::new();
        };

        // 清理已不在簿的撤单标记
        let live_ids: HashSet<OrderId> = symbol_state
            .pending_orders()
            .map(|p| p.order.id.clone())
            .collect();
        self.cancelling.retain(|id| live_ids.contains(id));

        let net_delta = greeks.delta + symbol_state.position_size(self.exchange);
        let pending = symbol_state.pending_orders().next().cloned();

        match self.desired_hedge(ref_price, net_delta) {
            // 带内：撤掉残留的在途对冲单
            None => pending
                .and_then(|p| self.cancel_confirmed(&p))
                .into_iter()
                .collect(),
            // 越带：维护单张对冲单
            Some((side, qty, price)) => self.maintain(pending, side, qty, price, net_delta),
        }
    }

    /// 维护单张对冲单。**不追价**：仅在方向翻转或数量显著漂移时撤单重挂。
    fn maintain(
        &mut self,
        pending: Option<PendingOrder>,
        side: Side,
        qty: Quantity,
        price: Price,
        net_delta: f64,
    ) -> Vec<OutcomeEvent> {
        match pending {
            Some(p) => match p.order.order_type {
                OrderType::Limit { .. } => {
                    let side_changed = p.order.side != side;
                    let qty_drift = (p.order.quantity - qty).abs() / qty > self.qty_tolerance_ratio;
                    if side_changed || qty_drift {
                        self.cancel_confirmed(&p).into_iter().collect()
                    } else {
                        Vec::new()
                    }
                }
                OrderType::Market => Vec::new(),
            },
            None => {
                if qty < self.min_hedge_qty {
                    Vec::new()
                } else {
                    vec![OutcomeEvent::PlaceOrders {
                        orders: vec![Order {
                            id: String::new(),
                            exchange: self.exchange,
                            symbol: self.symbol.clone(),
                            side,
                            order_type: OrderType::Limit {
                                price,
                                tif: TimeInForce::PostOnly,
                            },
                            quantity: qty,
                            reduce_only: false,
                            client_order_id: String::new(),
                        }],
                        comment: format!(
                            "gamma_hedge | {side:?} netDelta={net_delta:.4} qty={qty:.4} px={price:.4}"
                        ),
                    }]
                }
            }
        }
    }

    /// 撤掉已确认挂单 (Created 无 orderId 不能撤，等确认)；防止重复撤单。
    fn cancel_confirmed(&mut self, pending: &PendingOrder) -> Option<OutcomeEvent> {
        if pending.status.is_confirmed() && !self.cancelling.contains(&pending.order.id) {
            self.cancelling.insert(pending.order.id.clone());
            tracing::debug!(
                symbol = %self.symbol,
                order_id = %pending.order.id,
                side = ?pending.order.side,
                "gamma_hedge cancel (side flip / qty drift / back-in-band)"
            );
            Some(OutcomeEvent::CancelOrder {
                exchange: self.exchange,
                symbol: self.symbol.clone(),
                order_id: pending.order.id.clone(),
                client_order_id: pending.order.client_order_id.clone(),
            })
        } else {
            None
        }
    }
}

impl Strategy for GammaScalpStrategy {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        // 用 BBO 订阅仅为把 (exchange, symbol) 注册进路由/过滤范围；MarketTrade 事件按
        // (exchange, symbol) 路由即可送达 (本框架无独立的 Trade 订阅类型)。
        let mut kinds = HashSet::new();
        kinds.insert(SubscriptionKind::BBO {
            symbol: self.symbol.clone(),
        });
        let mut m = HashMap::new();
        m.insert(self.exchange, kinds);
        m
    }

    fn order_timeout_ms(&self) -> u64 {
        5000
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        match event {
            // 逐笔成交：以最新成交价为基准评估对冲
            IncomeEvent::Market(m) => match &m.data {
                MarketData::MarketTrade(t)
                    if t.exchange == self.exchange && t.symbol == self.symbol =>
                {
                    self.last_price = Some(t.price);
                    self.hedge(t.price, view)
                }
                _ => Vec::new(),
            },
            // greeks 变化 (delta 漂移) 也即时触发，用最新成交价为基准
            IncomeEvent::Account(a) => match &a.data {
                AccountData::Greeks(g) if g.exchange == self.exchange && g.ccy == self.ccy => {
                    match self.last_price {
                        Some(p) => self.hedge(p, view),
                        None => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strat() -> GammaScalpStrategy {
        // band=0.1, offset=0.2%
        GammaScalpStrategy::new(Exchange::Binance, "ETH".to_string(), "ETH".to_string(), 0.1, 0.002)
    }

    #[test]
    fn within_band_no_hedge() {
        let s = strat();
        assert!(s.desired_hedge(2000.0, 0.05).is_none());
        assert!(s.desired_hedge(2000.0, -0.1).is_none()); // 边界含等号: 不超过即不动
        assert!(s.desired_hedge(2000.0, 0.1).is_none());
    }

    #[test]
    fn long_delta_sells_above() {
        // 净 delta 偏多 -> 上方挂卖, qty=net_delta
        let (side, qty, px) = strat().desired_hedge(2000.0, 0.5).unwrap();
        assert_eq!(side, Side::Short);
        assert!((qty - 0.5).abs() < 1e-12);
        assert!((px - 2000.0 * 1.002).abs() < 1e-9); // 上方 +0.2%
    }

    #[test]
    fn short_delta_buys_below() {
        // 净 delta 偏空 -> 下方挂买, qty=-net_delta (正)
        let (side, qty, px) = strat().desired_hedge(2000.0, -0.5).unwrap();
        assert_eq!(side, Side::Long);
        assert!((qty - 0.5).abs() < 1e-12);
        assert!((px - 2000.0 * 0.998).abs() < 1e-9); // 下方 -0.2%
    }
}

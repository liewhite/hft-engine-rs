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

use crate::domain::{
    dec, from_f64, to_f64, Decimal, Exchange, OrderIntent, OrderType, Price, Quantity, Side,
    Symbol, TimeInForce,
};
use crate::exchange::{AccountStream, SubscriptionKind};
use crate::messaging::SymbolMarketData;
use crate::messaging::{AccountData, ExchangeAccountData, IncomeEvent, MarketData, PendingOrder};
use crate::strategy::{Signal, Strategy, StrategyView};
use std::collections::{HashMap, HashSet};

/// 对冲数量相对目标漂移超该比例则撤单重挂 (默认值)
const DEFAULT_QTY_TOLERANCE_RATIO: f64 = 0.2;
/// 最小对冲数量 (币本位)，低于此不挂单避免碎单 (默认值)
const DEFAULT_MIN_HEDGE_QTY: Quantity = dec!(0.001);

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
            last_price: None,
        }
    }

    /// 越带的目标对冲：把净 delta 拉回中性。偏多 -> 上方挂卖；偏空 -> 下方挂买。
    /// 返回 (方向, 数量, 挂单价)；带内返回 None。
    ///
    /// 净 delta 是希腊值统计量 (f64)，折成下单意图量 (下单时框架按步长取整)；间距比率折成
    /// Decimal 乘到价格上。折不出来 (非有限值) 时按带内处理，不下单。
    fn desired_hedge(&self, ref_price: Price, net_delta: f64) -> Option<(Side, Quantity, Price)> {
        let offset = from_f64(self.base_offset_ratio)?;
        if net_delta > self.delta_band {
            Some((
                Side::Short,
                from_f64(net_delta)?,
                ref_price * (Decimal::ONE + offset),
            ))
        } else if net_delta < -self.delta_band {
            Some((
                Side::Long,
                from_f64(-net_delta)?,
                ref_price * (Decimal::ONE - offset),
            ))
        } else {
            None
        }
    }

    fn hedge(&mut self, ref_price: Price, view: StrategyView<'_>) -> Vec<Signal> {
        let Some(symbol_state) = view.symbol(&self.symbol) else {
            return Vec::new();
        };
        // greeks 与 cashBal 均到达才动作 (delta 修正就绪)
        let Some(greeks) = view.greeks(self.exchange, &self.ccy) else {
            return Vec::new();
        };

        let net_delta = greeks.delta + to_f64(symbol_state.position_size(self.exchange));
        let pending = symbol_state.pending_orders().next().cloned();

        match self.desired_hedge(ref_price, net_delta) {
            // 带内：撤掉残留的在途对冲单
            None => pending.map(|p| self.cancel(&p)).into_iter().collect(),
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
    ) -> Vec<Signal> {
        match pending {
            Some(p) => match p.order.order_type {
                OrderType::Limit { .. } => {
                    let side_changed = p.order.side != side;
                    // 漂移比例是统计比率，按 f64 算 (也避开 Decimal 除零 panic)
                    let qty_drift = to_f64((p.order.quantity - qty).abs()) / to_f64(qty)
                        > self.qty_tolerance_ratio;
                    if side_changed || qty_drift {
                        vec![self.cancel(&p)]
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
                    vec![Signal::PlaceOrders {
                        orders: vec![OrderIntent {
                            exchange: self.exchange,
                            symbol: self.symbol.clone(),
                            side,
                            order_type: OrderType::Limit {
                                price,
                                tif: TimeInForce::PostOnly,
                            },
                            quantity: qty,
                            reduce_only: false,
                        }],
                        comment: format!(
                            "gamma_hedge | {side:?} netDelta={net_delta:.4} qty={qty:.4} px={price:.4}"
                        ),
                    }]
                }
            }
        }
    }

    /// 撤掉对冲单。下单还没受理、已结束、撤单还在途的单由框架丢弃（见 `Signal::CancelOrder`），这里不用记；
    /// 被丢弃的撤单在下一次重算（逐笔成交或 greeks 事件）时再发
    fn cancel(&self, pending: &PendingOrder) -> Signal {
        tracing::debug!(
            symbol = %self.symbol,
            client_order_id = %pending.order.client_order_id,
            side = ?pending.order.side,
            "gamma_hedge cancel (side flip / qty drift / back-in-band)"
        );
        Signal::CancelOrder {
            symbol: self.symbol.clone(),
            client_order_id: pending.order.client_order_id.clone(),
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

    /// 希腊值来自对冲所的账户读数（见 on_event 里按 exchange 过滤）
    fn account_streams(&self) -> HashMap<Exchange, HashSet<AccountStream>> {
        HashMap::from([(self.exchange, HashSet::from([AccountStream::Greeks]))])
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<Signal> {
        match event {
            // 逐笔成交：以最新成交价为基准评估对冲
            IncomeEvent::Market(m) => match &m.data {
                MarketData::Symbol(SymbolMarketData::MarketTrade(t))
                    if t.exchange == self.exchange && t.symbol == self.symbol =>
                {
                    self.last_price = Some(t.price);
                    self.hedge(t.price, view)
                }
                _ => Vec::new(),
            },
            // greeks 变化 (delta 漂移) 也即时触发，用最新成交价为基准
            IncomeEvent::Account(a) => match &a.data {
                AccountData::Exchange(ExchangeAccountData::Greeks(g))
                    if g.exchange == self.exchange && g.ccy == self.ccy =>
                {
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
        GammaScalpStrategy::new(
            Exchange::Binance,
            "ETH".to_string(),
            "ETH".to_string(),
            0.1,
            0.002,
        )
    }

    #[test]
    fn within_band_no_hedge() {
        let s = strat();
        assert!(s.desired_hedge(dec!(2000.0), 0.05).is_none());
        assert!(s.desired_hedge(dec!(2000.0), -0.1).is_none()); // 边界含等号: 不超过即不动
        assert!(s.desired_hedge(dec!(2000.0), 0.1).is_none());
    }

    #[test]
    fn long_delta_sells_above() {
        // 净 delta 偏多 -> 上方挂卖, qty=net_delta
        let (side, qty, px) = strat().desired_hedge(dec!(2000.0), 0.5).unwrap();
        assert_eq!(side, Side::Short);
        assert_eq!(qty, dec!(0.5));
        assert_eq!(px, dec!(2004)); // 上方 +0.2%
    }

    #[test]
    fn short_delta_buys_below() {
        // 净 delta 偏空 -> 下方挂买, qty=-net_delta (正)
        let (side, qty, px) = strat().desired_hedge(dec!(2000.0), -0.5).unwrap();
        assert_eq!(side, Side::Long);
        assert_eq!(qty, dec!(0.5));
        assert_eq!(px, dec!(1996)); // 下方 -0.2%
    }
}

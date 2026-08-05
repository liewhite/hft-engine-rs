use crate::domain::{
    Exchange, Fill, MarketTrade, Order, OrderId, OrderType, OrderUpdate, OrderStatus, Position,
    Price, Quantity, Side, Symbol, TimeInForce, Timestamp, BBO,
};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::sim::ledger::Ledger;
use crate::sim::matcher;
use indexmap::IndexMap;
use std::collections::HashMap;

/// 成交的流动性角色：maker (resting 单被越价成交) / taker (到达即吃单成交)，决定手续费率。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liquidity {
    Maker,
    Taker,
}

/// 挂单簿中的一张挂单。
#[derive(Debug, Clone)]
pub struct RestingOrder {
    pub order_id: OrderId,
    pub client_order_id: String,
    pub symbol: Symbol,
    pub side: Side,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub reduce_only: bool,
}

/// 虚拟柜台的全部状态 (账本 + 挂单簿 + 最新行情)。
///
/// # 边界：一份 `SimState` = **一个账户在一个交易所**的柜台
///
/// 交易所身份在构造时钉死（[`Self::exchange`]），内部一切状态（挂单簿、持仓、行情）都只属于
/// 这一个所 —— 跨所隔离由持有方按 (账户, 交易所) 分实例实现（模拟盘
/// `PaperCounterActor`、回测 `BacktestEngine` 皆如此）。此前 exchange 以方法参数传入、
/// 只用来给回报贴标签，内部键全按 symbol —— 接口看似有交易所维度实则没有：跨所策略绑到
/// 一个实例上，A 所挂单会被 B 所的成交撮合、两所持仓并进一条。收进构造参数后，
/// "喂错所的行情"成为可检测的路由 bug（见 [`Self::on_market`]），而非静默串账。
///
/// # 数量口径：全程**币本位**
///
/// 本状态机不参与单位折算，进出一律币本位 (见 [`crate::domain::Quantity`])：订单来自
/// `StrategyRunner`（已按交易所精度取整、仍是币本位），成交与持仓也以币本位落账。币 -> 交易所
/// 单位的折算只发生在实盘出口 ([`crate::exchange::ExchangeOrder`])，回测/模拟盘不经过那里。
///
/// 这一点必须成立，否则用 OKX 这类张数计价的所回测时，持仓与盈亏会静默偏差 contract_size 倍
/// （Binance 的 contract_size 为 1，掩盖过这个问题）。
///
/// 与 ox-demo `SimState` 同构，但作两处修正 (见 plan 的 Scala 问题清单)：
///   1. **确定性**：所有回流事件时间戳取调用方注入的虚拟 `now` / 行情时间戳，绝不取墙钟。
///   2. **撮合顺序**：`resting` 用插入序 `IndexMap`，越价成交按**到达顺序** (时间优先) 撮合，
///      而非依赖 HashMap 哈希序 (Rust 的 `HashMap` 迭代序进程内随机，照搬会破坏跨运行确定性)。
///
/// 转移采用 `&mut self -> Vec<IncomeEvent>` (就地变更，免 clone；纯度足够脱离线程单测)。
/// 每次转移的首个回流事件是转发行情，其后才是本次触发的成交/状态回报 (保证行情先于成交)。
#[derive(Debug, Clone)]
pub struct SimState {
    /// 本柜台所属的交易所（构造时钉死，见类型文档）
    pub exchange: Exchange,
    pub ledger: Ledger,
    pub resting: IndexMap<OrderId, RestingOrder>,
    pub last_bbo: HashMap<Symbol, BBO>,
    pub last_mark: HashMap<Symbol, f64>,
    pub last_trade: HashMap<Symbol, f64>,
    pub maker_fee_rate: f64,
    pub taker_fee_rate: f64,
}

impl SimState {
    pub fn empty(exchange: Exchange, cash: f64, maker_fee_rate: f64, taker_fee_rate: f64) -> Self {
        Self {
            exchange,
            ledger: Ledger::empty(cash),
            resting: IndexMap::new(),
            last_bbo: HashMap::new(),
            last_mark: HashMap::new(),
            last_trade: HashMap::new(),
            maker_fee_rate,
            taker_fee_rate,
        }
    }

    /// 估值价格：标记价 > BBO 中间价 > 最新成交价 (trade-only 行情用最新成交价估值)
    pub fn mark_of(&self, symbol: &Symbol) -> f64 {
        if let Some(m) = self.last_mark.get(symbol) {
            *m
        } else if let Some(b) = self.last_bbo.get(symbol) {
            b.mid_price()
        } else if let Some(t) = self.last_trade.get(symbol) {
            *t
        } else {
            0.0
        }
    }

    // ==================== 上游行情到达 (用于撮合) ====================

    /// 只更新盘口、**不撮合**。
    ///
    /// 用于"仅以真实成交判定撮合"的场景（模拟盘）：盘口只作两件事 —— 持仓估值，以及订单
    /// 到达时的可成交性判断（PostOnly 是否会吃单、市价单有无参考价）；成交与否一律由
    /// [`Self::on_market`] 收到的真实成交（trade-print）判定。
    ///
    /// 与 `on_market(BBO)` 的区别正在于此：后者会把"盘口越过挂单价"也算作成交，对挂单策略
    /// 偏乐观 —— 盘口触到你的价位只说明队列**前面**的人在成交。
    pub fn observe_bbo(&mut self, bbo: &BBO) {
        self.last_bbo.insert(bbo.symbol.clone(), bbo.clone());
    }

    /// 行情到达交易所：更新行情 + 撮合越价挂单。返回要回流策略的事件 (首个为转发行情)。
    ///
    /// 喂进不属于本所的行情是**路由 bug**（持有方应按所分发）：记 error 并只转发不撮合，
    /// 绝不拿别的所的价格去撮合本所的挂单。
    pub fn on_market(&mut self, ev: &IncomeEvent) -> Vec<IncomeEvent> {
        if let Some(ev_exchange) = ev.exchange() {
            if ev_exchange != self.exchange {
                tracing::error!(
                    counter_exchange = %self.exchange,
                    event_exchange = %ev_exchange,
                    "SimState 收到其他交易所的行情（路由 bug），只转发不撮合"
                );
                return vec![ev.clone()];
            }
        }
        match &ev.data {
            ExchangeEventData::BBO(bbo) => {
                self.last_bbo.insert(bbo.symbol.clone(), bbo.clone());
                let mut out = vec![ev.clone()];
                out.extend(self.match_crossing(&bbo.clone()));
                out
            }
            ExchangeEventData::MarketTrade(t) => {
                // trade-print 撮合：真实成交价严格越过挂单价即成交 (无 bbo 行情时的撮合来源)
                self.last_trade.insert(t.symbol.clone(), t.price);
                let mut out = vec![ev.clone()];
                out.extend(self.match_trade(&t.clone()));
                out
            }
            ExchangeEventData::MarkPrice(mp) => {
                self.last_mark.insert(mp.symbol.clone(), mp.price);
                vec![ev.clone()]
            }
            _ => vec![ev.clone()],
        }
    }

    /// BBO 越过挂单价的全部挂单成交 (maker 成交价取挂单价)，按到达顺序撮合。
    fn match_crossing(&mut self, bbo: &BBO) -> Vec<IncomeEvent> {
        self.fill_crossed(&bbo.symbol, bbo.timestamp, |o| {
            matcher::crosses(o.side, o.limit_price, bbo)
        })
    }

    /// 真实成交严格越过挂单价的全部挂单成交 (maker 成交价取挂单价)，按到达顺序撮合。
    fn match_trade(&mut self, t: &MarketTrade) -> Vec<IncomeEvent> {
        self.fill_crossed(&t.symbol, t.timestamp, |o| {
            matcher::trade_crosses(o.side, o.limit_price, t.price)
        })
    }

    /// 撮合公共形态：先按插入序 (到达序=时间优先) collect 满足 `crossed` 谓词的挂单，再逐一
    /// 出簿成交 (maker 价取挂单价)。先 collect 后变更，避免迭代中修改容器；逐一 `shift_remove`
    /// + `fill` 保证不漏单/不重复，且 reduceOnly 截断按成交先后顺序作用于当前持仓。
    fn fill_crossed(
        &mut self,
        symbol: &Symbol,
        ts: Timestamp,
        crossed: impl Fn(&RestingOrder) -> bool,
    ) -> Vec<IncomeEvent> {
        let matched: Vec<RestingOrder> = self
            .resting
            .values()
            .filter(|o| &o.symbol == symbol && crossed(o))
            .cloned()
            .collect();
        let mut out = Vec::new();
        for o in matched {
            self.resting.shift_remove(&o.order_id);
            out.extend(self.fill(
                &o.order_id,
                &o.client_order_id,
                &o.symbol,
                o.side,
                o.limit_price,
                o.quantity,
                ts,
                Liquidity::Maker,
                o.reduce_only,
            ));
        }
        out
    }

    // ==================== 下单到达撮合 ====================

    /// 订单到达撮合：按类型/TIF 决定 resting / 成交 / 拒单。
    /// `now` 为虚拟时间，仅在无 BBO 行情时作回报事件时间戳 (修正 ox-demo 的墙钟污染)。
    pub fn on_order_arrived(
        &mut self,
        now: Timestamp,
        order: &Order,
        order_id: &OrderId,
    ) -> Vec<IncomeEvent> {
        let bbo = self.last_bbo.get(&order.symbol).cloned();
        let ts = bbo.as_ref().map(|b| b.timestamp).unwrap_or(now);
        match &order.order_type {
            OrderType::Market => match &bbo {
                Some(b) => {
                    let price = matcher::touch_price(order.side, b);
                    self.fill(
                        order_id,
                        &order.client_order_id,
                        &order.symbol,
                        order.side,
                        price,
                        order.quantity,
                        ts,
                        Liquidity::Taker,
                        order.reduce_only,
                    )
                }
                None => vec![status_event(
                    self.exchange,
                    order,
                    order_id,
                    OrderStatus::Rejected {
                        reason: "no market data for market order".to_string(),
                    },
                    0.0,
                    ts,
                )],
            },
            OrderType::Limit { price: limit, tif } => {
                // 到达即可成交时的对手价 (None = 不可成交)。可成交性与 resting 越价同一判定 (crosses)。
                let taker_price: Option<Price> = bbo
                    .as_ref()
                    .filter(|b| matcher::crosses(order.side, *limit, b))
                    .map(|b| matcher::touch_price(order.side, b));
                match tif {
                    TimeInForce::PostOnly => match taker_price {
                        Some(_) => vec![status_event(
                            self.exchange,
                            order,
                            order_id,
                            OrderStatus::Rejected {
                                reason: "post-only would take liquidity".to_string(),
                            },
                            *limit,
                            ts,
                        )],
                        None => self.rest(order, order_id, *limit, ts),
                    },
                    TimeInForce::GTC => match taker_price {
                        Some(p) => self.fill(
                            order_id,
                            &order.client_order_id,
                            &order.symbol,
                            order.side,
                            p,
                            order.quantity,
                            ts,
                            Liquidity::Taker,
                            order.reduce_only,
                        ),
                        None => self.rest(order, order_id, *limit, ts),
                    },
                    TimeInForce::IOC | TimeInForce::FOK => match taker_price {
                        // 无深度模型, 可成交即全量成交, 否则整单取消 (不 resting)
                        Some(p) => self.fill(
                            order_id,
                            &order.client_order_id,
                            &order.symbol,
                            order.side,
                            p,
                            order.quantity,
                            ts,
                            Liquidity::Taker,
                            order.reduce_only,
                        ),
                        None => vec![status_event(
                            self.exchange,
                            order,
                            order_id,
                            OrderStatus::Cancelled,
                            *limit,
                            ts,
                        )],
                    },
                }
            }
        }
    }

    /// 撤单到达撮合：仍在簿则移除并回报 Cancelled；已成交 (不在簿) 则无事发生。
    pub fn on_cancel_arrived(
        &mut self,
        now: Timestamp,
        order_id: &OrderId,
    ) -> Vec<IncomeEvent> {
        match self.resting.shift_remove(order_id) {
            Some(o) => vec![ev_at(
                now,
                ExchangeEventData::OrderUpdate(OrderUpdate {
                    order_id: order_id.clone(),
                    client_order_id: Some(o.client_order_id),
                    exchange: self.exchange,
                    symbol: o.symbol,
                    side: o.side,
                    status: OrderStatus::Cancelled,
                    price: o.limit_price,
                    reduce_only: o.reduce_only,
                    quantity: o.quantity,
                    filled_quantity: 0.0,
                    fill_sz: 0.0,
                    timestamp: now,
                }),
            )],
            None => Vec::new(),
        }
    }

    // ==================== 私有构造 ====================

    fn rest(
        &mut self,
        order: &Order,
        order_id: &OrderId,
        limit: Price,
        ts: Timestamp,
    ) -> Vec<IncomeEvent> {
        self.resting.insert(
            order_id.clone(),
            RestingOrder {
                order_id: order_id.clone(),
                client_order_id: order.client_order_id.clone(),
                symbol: order.symbol.clone(),
                side: order.side,
                limit_price: limit,
                quantity: order.quantity,
                reduce_only: order.reduce_only,
            },
        );
        vec![status_event(self.exchange, order, order_id, OrderStatus::Pending, limit, ts)]
    }

    /// 成交落账。reduceOnly 单按当前持仓截断 (卖只平多、买只平空)，撮合层强制不反向开仓；
    /// 无可平仓位时不成交，回 Cancelled (订单已被调用方移出簿 / 不入簿)。
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        order_id: &OrderId,
        client_order_id: &str,
        symbol: &Symbol,
        side: Side,
        fill_price: Price,
        qty: Quantity,
        ts: Timestamp,
        liquidity: Liquidity,
        reduce_only: bool,
    ) -> Vec<IncomeEvent> {
        let effective_qty = if !reduce_only {
            qty
        } else {
            let pos_size = self.ledger.positions.get(symbol).map(|p| p.size).unwrap_or(0.0);
            match side {
                Side::Short => qty.min(pos_size.max(0.0)),    // 卖平多: 至多平掉现有多头
                Side::Long => qty.min((-pos_size).max(0.0)),  // 买平空: 至多平掉现有空头
            }
        };

        if reduce_only && effective_qty <= Position::EPSILON {
            // reduceOnly 无可平仓位 -> 不成交，回 Cancelled
            return vec![ev_at(
                ts,
                ExchangeEventData::OrderUpdate(OrderUpdate {
                    order_id: order_id.clone(),
                    client_order_id: Some(client_order_id.to_string()),
                    exchange: self.exchange,
                    symbol: symbol.clone(),
                    side,
                    status: OrderStatus::Cancelled,
                    price: fill_price,
                    reduce_only,
                    quantity: qty,
                    filled_quantity: 0.0,
                    fill_sz: 0.0,
                    timestamp: ts,
                }),
            )];
        }

        let fee_rate = match liquidity {
            Liquidity::Maker => self.maker_fee_rate,
            Liquidity::Taker => self.taker_fee_rate,
        };
        let fee = fill_price * effective_qty * fee_rate;
        self.ledger
            .apply_fill(self.exchange, symbol, side, fill_price, effective_qty, fee);

        let update = OrderUpdate {
            order_id: order_id.clone(),
            client_order_id: Some(client_order_id.to_string()),
            exchange: self.exchange,
            symbol: symbol.clone(),
            side,
            status: OrderStatus::Filled,
            price: fill_price,
            reduce_only,
            quantity: effective_qty,
            filled_quantity: effective_qty,
            fill_sz: effective_qty,
            timestamp: ts,
        };
        let f = Fill {
            exchange: self.exchange,
            symbol: symbol.clone(),
            side,
            price: fill_price,
            size: effective_qty,
            client_order_id: Some(client_order_id.to_string()),
            order_id: order_id.clone(),
            timestamp: ts,
            fee,
            reason: crate::domain::FillReason::Normal, // 回测无强平/ADL
        };
        vec![
            ev_at(ts, ExchangeEventData::OrderUpdate(update)),
            ev_at(ts, ExchangeEventData::Fill(f)),
        ]
    }
}

/// 构造回流事件：exchange_ts == local_ts == ts (一律取确定性时间，杜绝墙钟)。
fn ev_at(ts: Timestamp, data: ExchangeEventData) -> IncomeEvent {
    IncomeEvent {
        exchange_ts: ts,
        local_ts: ts,
        data,
    }
}

fn status_event(
    exchange: Exchange,
    order: &Order,
    order_id: &OrderId,
    status: OrderStatus,
    price: Price,
    ts: Timestamp,
) -> IncomeEvent {
    ev_at(
        ts,
        ExchangeEventData::OrderUpdate(OrderUpdate {
            order_id: order_id.clone(),
            client_order_id: Some(order.client_order_id.clone()),
            exchange,
            symbol: order.symbol.clone(),
            side: order.side,
            status,
            price,
            reduce_only: order.reduce_only,
            quantity: order.quantity,
            filled_quantity: 0.0,
            fill_sz: 0.0,
            timestamp: ts,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EX: Exchange = Exchange::Binance;
    fn sym() -> Symbol {
        "BTCUSDT".to_string()
    }
    fn empty() -> SimState {
        SimState::empty(EX, 10_000.0, 0.0, 0.0)
    }

    fn bbo(bid: Price, ask: Price, ts: Timestamp) -> BBO {
        BBO {
            exchange: EX,
            symbol: sym(),
            bid_price: bid,
            bid_qty: 1.0,
            ask_price: ask,
            ask_qty: 1.0,
            timestamp: ts,
        }
    }
    fn market_ev(b: BBO) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: b.timestamp,
            local_ts: b.timestamp,
            data: ExchangeEventData::BBO(b),
        }
    }
    fn trade_ev(price: Price, ts: Timestamp) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: ExchangeEventData::MarketTrade(MarketTrade {
                exchange: EX,
                symbol: sym(),
                price,
                qty: 1.0,
                is_buyer_maker: false,
                timestamp: ts,
            }),
        }
    }
    fn limit(side: Side, price: Price, tif: TimeInForce, cid: &str) -> Order {
        Order {
            id: String::new(),
            exchange: EX,
            symbol: sym(),
            side,
            order_type: OrderType::Limit { price, tif },
            quantity: 0.002,
            reduce_only: false,
            client_order_id: cid.to_string(),
        }
    }
    fn limit_ro(side: Side, price: Price, tif: TimeInForce, cid: &str, qty: Quantity) -> Order {
        Order {
            id: String::new(),
            exchange: EX,
            symbol: sym(),
            side,
            order_type: OrderType::Limit { price, tif },
            quantity: qty,
            reduce_only: true,
            client_order_id: cid.to_string(),
        }
    }
    fn statuses(evs: &[IncomeEvent]) -> Vec<OrderStatus> {
        evs.iter()
            .filter_map(|e| match &e.data {
                ExchangeEventData::OrderUpdate(u) => Some(u.status.clone()),
                _ => None,
            })
            .collect()
    }
    fn fills(evs: &[IncomeEvent]) -> Vec<Fill> {
        evs.iter()
            .filter_map(|e| match &e.data {
                ExchangeEventData::Fill(f) => Some(f.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn non_marketable_postonly_buy_rests() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        let evs = s.on_order_arrived(1, &limit(Side::Long, 49995.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        assert_eq!(statuses(&evs), vec![OrderStatus::Pending]);
        assert!(s.resting.contains_key("1"));
        assert!(fills(&evs).is_empty());
    }

    #[test]
    fn marketable_postonly_rejected() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        let evs = s.on_order_arrived(1, &limit(Side::Long, 50001.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        assert!(statuses(&evs).iter().any(|st| matches!(st, OrderStatus::Rejected { .. })));
        assert!(s.resting.is_empty());
        assert!(fills(&evs).is_empty());
    }

    #[test]
    fn resting_buy_crossed_fills_at_limit() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        s.on_order_arrived(1, &limit(Side::Long, 49995.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        let evs = s.on_market(&market_ev(bbo(49990.0, 49994.0, 2))); // ask 49994 <= 49995
        let f = fills(&evs);
        assert_eq!(f.iter().map(|x| x.price).collect::<Vec<_>>(), vec![49995.0]); // maker 价
        assert_eq!(s.resting.len(), 0);
        assert_eq!(s.ledger.positions[&sym()].size, 0.002);
    }

    #[test]
    fn market_event_precedes_fill() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        s.on_order_arrived(1, &limit(Side::Long, 49995.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        let evs = s.on_market(&market_ev(bbo(49990.0, 49994.0, 2)));
        assert!(matches!(evs[0].data, ExchangeEventData::BBO(_)), "首事件应为行情转发");
        assert!(evs[1..].iter().any(|e| matches!(e.data, ExchangeEventData::Fill(_))), "成交回报排在行情之后");
    }

    #[test]
    fn gtc_marketable_taker_fills_at_touch() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        let evs = s.on_order_arrived(1, &limit(Side::Long, 50005.0, TimeInForce::GTC, "b1"), &"1".to_string());
        assert_eq!(fills(&evs).iter().map(|f| f.price).collect::<Vec<_>>(), vec![50001.0]); // 吃卖价
        assert!(s.resting.is_empty());
    }

    #[test]
    fn ioc_non_marketable_cancelled() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        let evs = s.on_order_arrived(1, &limit(Side::Long, 49000.0, TimeInForce::IOC, "b1"), &"1".to_string());
        assert_eq!(statuses(&evs), vec![OrderStatus::Cancelled]);
        assert!(s.resting.is_empty());
    }

    #[test]
    fn cancel_arrived_removes_and_reports() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        s.on_order_arrived(1, &limit(Side::Long, 49995.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        let evs = s.on_cancel_arrived(2, &"1".to_string());
        assert_eq!(statuses(&evs), vec![OrderStatus::Cancelled]);
        assert!(s.resting.is_empty());
    }

    #[test]
    fn cancel_arrived_not_in_book_noop() {
        let mut s = empty();
        let evs = s.on_cancel_arrived(1, &"404".to_string());
        assert!(evs.is_empty());
    }

    #[test]
    fn reduce_only_sell_no_long_cancelled_no_reverse() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        // 上方 resting 卖单
        s.on_order_arrived(1, &limit_ro(Side::Short, 50010.0, TimeInForce::PostOnly, "s1", 0.002), &"1".to_string());
        let evs = s.on_market(&market_ev(bbo(50011.0, 50012.0, 2))); // bid 50011 >= 50010 -> 越价
        assert!(fills(&evs).is_empty(), "无多头可平, 不应成交");
        assert!(statuses(&evs).contains(&OrderStatus::Cancelled), "reduceOnly 无可平 -> Cancelled");
        assert!(s.ledger.positions.get(&sym()).map(|p| p.is_empty()).unwrap_or(true), "不得反向开出空头");
    }

    #[test]
    fn reduce_only_sell_truncated_to_position() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        s.on_order_arrived(1, &limit(Side::Long, 50005.0, TimeInForce::GTC, "b1"), &"1".to_string()); // taker 开多 0.002
        s.on_order_arrived(1, &limit_ro(Side::Short, 50010.0, TimeInForce::PostOnly, "s1", 0.005), &"2".to_string()); // 平仓量 > 持仓
        let evs = s.on_market(&market_ev(bbo(50011.0, 50012.0, 2)));
        assert_eq!(fills(&evs).iter().map(|f| f.size).collect::<Vec<_>>(), vec![0.002]); // 截断到多头
        assert_eq!(s.ledger.positions[&sym()].size, 0.0); // 平至 0, 不反手
    }

    #[test]
    fn observe_bbo_updates_quote_without_matching() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1)));
        s.on_order_arrived(1, &limit(Side::Long, 49995.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        // 盘口越过挂单价（ask 49994 <= 49995），但 observe_bbo 不撮合
        s.observe_bbo(&bbo(49990.0, 49994.0, 2));
        assert_eq!(s.resting.len(), 1, "observe_bbo 不应撮合");
        assert!(s.ledger.positions.get(&sym()).map(|p| p.is_empty()).unwrap_or(true));
        // 估值口径已更新
        assert_eq!(s.mark_of(&sym()), (49990.0 + 49994.0) / 2.0);
        // 真实成交穿过挂单价才成交
        let evs = s.on_market(&trade_ev(49994.0, 3));
        assert_eq!(fills(&evs).iter().map(|f| f.price).collect::<Vec<_>>(), vec![49995.0]);
        assert_eq!(s.resting.len(), 0);
    }

    #[test]
    fn trade_print_crossing_fills() {
        let mut s = empty();
        // 无 BBO，挂买单 @100 (limit 到达时无行情 -> resting)
        s.on_order_arrived(1, &limit(Side::Long, 100.0, TimeInForce::GTC, "b1"), &"1".to_string());
        assert!(s.resting.contains_key("1"));
        // 成交价 99 < 100 -> 越价成交 @100 (maker)
        let evs = s.on_market(&trade_ev(99.0, 2));
        let f = fills(&evs);
        assert_eq!(f.iter().map(|x| x.price).collect::<Vec<_>>(), vec![100.0]);
        assert_eq!(s.ledger.positions[&sym()].size, 0.002);
    }

    // ===== 手续费 (对应 SimStateFeeSpec) =====
    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b} got {a}");
    }

    #[test]
    fn taker_fee_charged() {
        let mut s = SimState::empty(EX, 10_000.0, 0.001, 0.002);
        s.on_market(&market_ev(bbo(100.0, 100.0, 1)));
        let mut o = limit(Side::Long, 0.0, TimeInForce::GTC, "o1");
        o.order_type = OrderType::Market;
        o.quantity = 2.0;
        s.on_order_arrived(1, &o, &"o1".to_string());
        near(s.ledger.cash, 10_000.0 - 0.4); // 100*2*0.002
        near(s.ledger.positions[&sym()].size, 2.0);
    }

    #[test]
    fn maker_fee_charged() {
        let mut s = SimState::empty(EX, 10_000.0, 0.001, 0.002);
        s.on_market(&market_ev(bbo(100.0, 100.0, 1)));
        let mut o = limit(Side::Long, 99.0, TimeInForce::GTC, "o1");
        o.quantity = 2.0;
        s.on_order_arrived(1, &o, &"o1".to_string());
        assert!(!s.resting.is_empty());
        near(s.ledger.cash, 10_000.0); // resting 不扣费
        s.on_market(&market_ev(bbo(98.0, 98.0, 2))); // ask 98 <= 99 -> maker 成交 @99
        near(s.ledger.cash, 10_000.0 - 99.0 * 2.0 * 0.001);
        near(s.ledger.positions[&sym()].size, 2.0);
    }
}

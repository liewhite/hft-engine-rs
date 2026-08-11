use crate::domain::{
    Exchange, Fill, MarketTrade, Order, OrderId, OrderType, OrderUpdate, OrderStatus, Position,
    Price, Quantity, Side, Symbol, SymbolMeta, TimeInForce, Timestamp, BBO,
};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::sim::ledger::Ledger;
use crate::sim::matcher;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::Arc;

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
    /// 原始委托量（不随成交递减；剩余量 = quantity - filled）
    pub quantity: Quantity,
    /// 累计成交量
    pub filled: Quantity,
    pub reduce_only: bool,
}

impl RestingOrder {
    /// 剩余未成交量
    pub fn remaining(&self) -> Quantity {
        (self.quantity - self.filled).max(0.0)
    }
}

/// 一次撮合尝试的结局（供撮合循环决定出簿/留簿与预算消耗）
enum FillResult {
    /// 成交了 `effective`；`done` = 累计已达原始委托量（应出簿）
    Traded { effective: Quantity, done: bool },
    /// reduce-only 无可平仓位，整单作废（应出簿，不消耗预算）
    NoPositionToReduce,
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
    /// 市场规则来源（下界校验 + 数量取整）。柜台像真实交易所一样强制执行市场规则 ——
    /// 这是三条驱动（实盘出口 / 模拟盘 / 回测）共享的唯一校验点安放处，见
    /// [`Self::on_order_arrived`]。键含 Exchange 只为与调用方（回测引擎、模拟柜台）持有的
    /// 全量表同形，免去每所过滤一份的转换；查询一律用 `self.exchange`。
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
}

impl SimState {
    pub fn empty(
        exchange: Exchange,
        cash: f64,
        maker_fee_rate: f64,
        taker_fee_rate: f64,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    ) -> Self {
        Self {
            exchange,
            ledger: Ledger::empty(cash),
            resting: IndexMap::new(),
            last_bbo: HashMap::new(),
            last_mark: HashMap::new(),
            last_trade: HashMap::new(),
            maker_fee_rate,
            taker_fee_rate,
            symbol_metas,
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
    ///
    /// 盘口路径**不限量**（bid/ask qty 是盘口挂量而非成交量，语义不同）——本就是偏乐观的
    /// 模型，见 `observe_bbo` 的说明。
    fn match_crossing(&mut self, bbo: &BBO) -> Vec<IncomeEvent> {
        self.fill_crossed(&bbo.symbol, bbo.timestamp, None, |o| {
            matcher::crosses(o.side, o.limit_price, bbo)
        })
    }

    /// 真实成交严格越过挂单价的挂单成交 (maker 成交价取挂单价)，按到达顺序撮合，
    /// **成交量以该笔 print 的数量为预算**：一笔 0.001 的成交只能吃掉 0.001 的挂单量，
    /// 吃不完的部分留在簿里（PartiallyFilled）。此前 qty 被完全忽略 —— 任意小的 print
    /// 可以吃掉全部越价挂单，成交量被系统性高估。
    fn match_trade(&mut self, t: &MarketTrade) -> Vec<IncomeEvent> {
        self.fill_crossed(&t.symbol, t.timestamp, Some(t.qty), |o| {
            matcher::trade_crosses(o.side, o.limit_price, t.price)
        })
    }

    /// 撮合公共形态：先按插入序 (到达序=时间优先) collect 满足 `crossed` 谓词的挂单，再
    /// 逐一成交 (maker 价取挂单价)。先 collect 后变更，避免迭代中修改容器。
    /// `budget` = 本轮可消耗的成交量上限（None = 不限量，BBO 路径）；按到达顺序分配，
    /// 耗尽即止 —— 时间优先的队列近似。
    fn fill_crossed(
        &mut self,
        symbol: &Symbol,
        ts: Timestamp,
        budget: Option<Quantity>,
        crossed: impl Fn(&RestingOrder) -> bool,
    ) -> Vec<IncomeEvent> {
        let matched: Vec<RestingOrder> = self
            .resting
            .values()
            .filter(|o| &o.symbol == symbol && o.remaining() > Position::EPSILON && crossed(o))
            .cloned()
            .collect();
        let mut remaining_budget = budget;
        let mut out = Vec::new();
        for o in matched {
            let take = match remaining_budget {
                Some(b) if b <= Position::EPSILON => break, // 预算耗尽
                Some(b) => o.remaining().min(b),
                None => o.remaining(),
            };
            let (result, events) = self.fill(
                &o.order_id,
                &o.client_order_id,
                &o.symbol,
                o.side,
                o.limit_price,
                o.quantity,
                o.filled,
                take,
                ts,
                Liquidity::Maker,
                o.reduce_only,
            );
            out.extend(events);
            match result {
                FillResult::Traded { effective, done } => {
                    if let Some(b) = remaining_budget.as_mut() {
                        *b -= effective;
                    }
                    if done {
                        self.resting.shift_remove(&o.order_id);
                    } else if let Some(entry) = self.resting.get_mut(&o.order_id) {
                        entry.filled += effective;
                    }
                }
                // reduce-only 无可平：整单作废出簿，不消耗预算
                FillResult::NoPositionToReduce => {
                    self.resting.shift_remove(&o.order_id);
                }
            }
        }
        out
    }

    // ==================== 下单到达撮合 ====================

    /// 订单到达撮合：按类型/TIF 决定 resting / 成交 / 拒单。
    ///
    /// # 可成交性的参考价：盘口对手价，缺盘口退化用最新成交价
    ///
    /// 此前只认 BBO —— trade-native 行情（默认回测路径、模拟盘只订成交流时）下：
    /// 市价单永远被拒、PostOnly 永不被拒（穿价挂单照挂、还按 maker 费成交）、深度穿价的
    /// GTC 挂进簿里按 limit 价成交。三者都系统性偏离真实交易所行为，且方向上一致高估
    /// 挂单策略的收益（回测调好的参数上线即漂移）。
    ///
    /// 回报时间戳一律用到达时刻 `now`：此前有盘口时取"最后一条 BBO 的 ts"，行情停推时
    /// 订单回报的时间被冻在过去，订单超时判定（按回报时间起算）随之失效。
    ///
    /// # 市场规则在柜台强制执行（与真实交易所一致）
    ///
    /// 数量下界（取整为 0 / 低于 `min_order_size`）与精度取整在此校验，规则出处是
    /// [`SymbolMeta::checked_exchange_qty`] —— 与实盘出口
    /// [`crate::exchange::ExchangeOrder::from_domain`] 同一个函数。这保证**实盘必拒的单，
    /// 模拟盘与回测也拒**；此前回测路径没有这道校验，小额残单在回测里照常成交、
    /// 实盘却被拒 —— 回测调好的参数上线即漂移。缺 SymbolMeta 同样拒单（真实交易所
    /// 不存在"没有元数据的合约"，缺失属装配错误，应当刺眼）。
    pub fn on_order_arrived(
        &mut self,
        now: Timestamp,
        order: &Order,
        order_id: &OrderId,
    ) -> Vec<IncomeEvent> {
        // 与 on_market 同一守卫：喂错所的订单是路由 bug，拒单并记 error ——
        // 绝不拿本所的行情去撮合别的所的订单
        if order.exchange != self.exchange {
            tracing::error!(
                counter_exchange = %self.exchange,
                order_exchange = %order.exchange,
                order_id = %order_id,
                "SimState 收到其他交易所的订单（路由 bug），拒单"
            );
            return vec![status_event(
                self.exchange,
                order,
                order_id,
                OrderStatus::Rejected {
                    reason: "order routed to wrong exchange counter".to_string(),
                },
                now,
            )];
        }
        // 市场规则校验 + 按交易所精度向下取整（策略请求 0.0016 而 step=0.001 时，
        // 实盘只会成交 0.001，柜台必须同量 —— 否则模拟盘每单多成交至多一个 step）
        let key = (self.exchange, order.symbol.clone());
        let checked = match self.symbol_metas.get(&key) {
            None => Err(format!(
                "no SymbolMeta for {}/{}",
                order.exchange, order.symbol
            )),
            Some(meta) => meta
                .checked_exchange_qty(order.quantity)
                .map(|_| meta.round_coin_size_down(order.quantity)),
        };
        let order = &match checked {
            Ok(quantity) => Order {
                quantity,
                ..order.clone()
            },
            Err(reason) => {
                return vec![status_event(
                    self.exchange,
                    order,
                    order_id,
                    OrderStatus::Rejected { reason },
                    now,
                )];
            }
        };
        // taker 参考价：对手价 > 最新成交价；两者皆无 = 无从判定可成交性
        let reference: Option<Price> = match self.last_bbo.get(&order.symbol) {
            Some(b) => Some(matcher::touch_price(order.side, b)),
            None => self.last_trade.get(&order.symbol).copied(),
        };
        let ts = now;
        match &order.order_type {
            OrderType::Market => match reference {
                Some(price) => self.taker_fill(order, order_id, price, ts),
                None => vec![status_event(
                    self.exchange,
                    order,
                    order_id,
                    OrderStatus::Rejected {
                        reason: "no market data for market order".to_string(),
                    },
                    ts,
                )],
            },
            OrderType::Limit { price: limit, tif } => {
                // 到达即可成交时的成交价 (None = 不可成交)。判定与 resting 越价同口径
                // （marketable_at 含等，见 matcher）。
                let taker_price: Option<Price> =
                    reference.filter(|r| matcher::marketable_at(order.side, *limit, *r));
                match tif {
                    TimeInForce::PostOnly => match taker_price {
                        Some(_) => vec![status_event(self.exchange,
                            order,
                            order_id,
                            OrderStatus::Rejected {
                                reason: "post-only would take liquidity".to_string(),
                            },
                            ts,
                        )],
                        None => self.rest(order, order_id, *limit, ts),
                    },
                    TimeInForce::GTC => match taker_price {
                        Some(p) => self.taker_fill(order, order_id, p, ts),
                        None => self.rest(order, order_id, *limit, ts),
                    },
                    TimeInForce::IOC | TimeInForce::FOK => match taker_price {
                        // 无深度模型, 可成交即全量成交, 否则整单取消 (不 resting)
                        Some(p) => self.taker_fill(order, order_id, p, ts),
                        None => vec![status_event(
                            self.exchange,
                            order,
                            order_id,
                            OrderStatus::Cancelled,
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
                    quantity: o.quantity,
                    // 如实带上撤单前的累计成交（部分成交后撤单），不谎报 0
                }),
            )],
            None => Vec::new(),
        }
    }

    // ==================== 私有构造 ====================

    /// taker 路径（订单到达即成交，**不 resting**）。成交后若有剩余 —— 在无深度模型下
    /// 只可能来自 reduceOnly 截断 —— 当场作废剩余量并补一条 Cancelled 终态：taker 单
    /// 没有簿可留，回报必须以终态闭合，否则策略的 pending 永远清不掉。
    fn taker_fill(
        &mut self,
        order: &Order,
        order_id: &OrderId,
        price: Price,
        ts: Timestamp,
    ) -> Vec<IncomeEvent> {
        let (result, mut events) = self.fill(
            order_id,
            &order.client_order_id,
            &order.symbol,
            order.side,
            price,
            order.quantity,
            0.0,
            order.quantity,
            ts,
            Liquidity::Taker,
            order.reduce_only,
        );
        if let FillResult::Traded { done: false, .. } = result
        {
            events.push(ev_at(
                ts,
                ExchangeEventData::OrderUpdate(OrderUpdate {
                    order_id: order_id.clone(),
                    client_order_id: Some(order.client_order_id.clone()),
                    exchange: self.exchange,
                    symbol: order.symbol.clone(),
                    side: order.side,
                    status: OrderStatus::Cancelled,
                    quantity: order.quantity,
                }),
            ));
        }
        events
    }

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
                filled: 0.0,
                reduce_only: order.reduce_only,
            },
        );
        vec![status_event(self.exchange, order, order_id, OrderStatus::Pending, ts)]
    }

    /// 尝试成交 `take` 数量并落账。
    ///
    /// - `order_qty` 是**原始委托量**、`already_filled` 是本次之前的累计成交 —— 回报字段
    ///   如实区分三个量：`quantity` = 原始量、`filled_quantity` = 累计、`fill_sz` = 本次。
    ///   （此前把截断后的量谎报成订单总量，策略比对"我下了多少"会得到不一致视图。）
    /// - 累计达到原始量即 `Filled`，否则 `PartiallyFilled`；出簿/留簿由调用方按
    ///   [`FillResult`] 决定。
    /// - reduceOnly 单按当前持仓截断 (卖只平多、买只平空)，撮合层强制不反向开仓；
    ///   无可平仓位时不成交，回 Cancelled（[`FillResult::NoPositionToReduce`]，调用方出簿）。
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        order_id: &OrderId,
        client_order_id: &str,
        symbol: &Symbol,
        side: Side,
        fill_price: Price,
        order_qty: Quantity,
        already_filled: Quantity,
        take: Quantity,
        ts: Timestamp,
        liquidity: Liquidity,
        reduce_only: bool,
    ) -> (FillResult, Vec<IncomeEvent>) {
        let effective_qty = if !reduce_only {
            take
        } else {
            let pos_size = self.ledger.positions.get(symbol).map(|p| p.size).unwrap_or(0.0);
            match side {
                Side::Short => take.min(pos_size.max(0.0)),    // 卖平多: 至多平掉现有多头
                Side::Long => take.min((-pos_size).max(0.0)),  // 买平空: 至多平掉现有空头
            }
        };

        if reduce_only && effective_qty <= Position::EPSILON {
            // reduceOnly 无可平仓位 -> 不成交，回 Cancelled
            let events = vec![ev_at(
                ts,
                ExchangeEventData::OrderUpdate(OrderUpdate {
                    order_id: order_id.clone(),
                    client_order_id: Some(client_order_id.to_string()),
                    exchange: self.exchange,
                    symbol: symbol.clone(),
                    side,
                    status: OrderStatus::Cancelled,
                    quantity: order_qty,
                }),
            )];
            return (FillResult::NoPositionToReduce, events);
        }

        let fee_rate = match liquidity {
            Liquidity::Maker => self.maker_fee_rate,
            Liquidity::Taker => self.taker_fee_rate,
        };
        let fee = fill_price * effective_qty * fee_rate;
        self.ledger
            .apply_fill(symbol, side, fill_price, effective_qty, fee);

        let cumulative = already_filled + effective_qty;
        let done = cumulative + Position::EPSILON >= order_qty;
        let status = if done {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        let update = OrderUpdate {
            order_id: order_id.clone(),
            client_order_id: Some(client_order_id.to_string()),
            exchange: self.exchange,
            symbol: symbol.clone(),
            side,
            status,
            quantity: order_qty,
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
        let events = vec![
            ev_at(ts, ExchangeEventData::OrderUpdate(update)),
            ev_at(ts, ExchangeEventData::Fill(f)),
        ];
        (
            FillResult::Traded {
                effective: effective_qty,
                done,
            },
            events,
        )
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
            quantity: order.quantity,
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
    /// 测试元数据：步长/最小量取得足够小，既有测试的数量不触发下界校验
    fn test_metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        use crate::exchange::utils::StepFormatter;
        Arc::new(HashMap::from([(
            (EX, sym()),
            SymbolMeta {
                exchange: EX,
                symbol: sym(),
                price_formatter: Arc::new(StepFormatter::new(0.001)),
                size_step: 0.001,
                min_order_size: 0.001,
                contract_size: 1.0,
            },
        )]))
    }
    fn empty() -> SimState {
        SimState::empty(EX, 10_000.0, 0.0, 0.0, test_metas())
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
        trade_ev_qty(price, 1.0, ts)
    }
    fn trade_ev_qty(price: Price, qty: Quantity, ts: Timestamp) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: ExchangeEventData::MarketTrade(MarketTrade {
                exchange: EX,
                symbol: sym(),
                price,
                qty,
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

    // ===== 部分成交：print 数量作为撮合预算 =====

    /// 一笔 print 的数量按**到达顺序**在越价挂单间分配，耗尽即止；吃剩的量留簿。
    /// 回报字段如实区分：quantity=原始量、filled_quantity=累计、fill_sz=本次。
    #[test]
    fn trade_qty_is_consumed_across_orders_in_arrival_order() {
        let mut s = empty();
        // 两张买单先后到达：0.7 @100、0.5 @101（都无行情时 resting）
        let mut o1 = limit(Side::Long, 100.0, TimeInForce::GTC, "b1");
        o1.quantity = 0.7;
        let mut o2 = limit(Side::Long, 101.0, TimeInForce::GTC, "b2");
        o2.quantity = 0.5;
        s.on_order_arrived(1, &o1, &"1".to_string());
        s.on_order_arrived(1, &o2, &"2".to_string());
        // print 1.0 @99 越过两张单：先到的 b1 全吃 0.7，剩余 0.3 给 b2
        let evs = s.on_market(&trade_ev_qty(99.0, 1.0, 2));
        let f = fills(&evs);
        let sizes: Vec<(String, f64)> =
            f.iter().map(|x| (x.order_id.clone(), x.size)).collect();
        assert_eq!(sizes.len(), 2, "预算应分给两张单: {sizes:?}");
        assert_eq!(sizes[0].0, "1");
        near(sizes[0].1, 0.7);
        assert_eq!(sizes[1].0, "2");
        near(sizes[1].1, 0.3);
        // b1 全成出簿；b2 剩 0.2 留簿，回报 PartiallyFilled(累计 0.3)
        assert!(!s.resting.contains_key("1"));
        near(s.resting.get("2").map(|o| o.remaining()).unwrap_or(0.0), 0.2);
        assert!(statuses(&evs).contains(&OrderStatus::Filled));
        assert!(
            statuses(&evs)
                .iter()
                .any(|st| matches!(st, OrderStatus::PartiallyFilled)),
            "应有 PartiallyFilled 回报: {:?}",
            statuses(&evs)
        );
        // 下一笔 print 足量吃完 b2 剩余，终态 Filled、累计=原始量
        let evs = s.on_market(&trade_ev_qty(99.0, 0.25, 3));
        assert!(statuses(&evs).contains(&OrderStatus::Filled));
        assert!(s.resting.is_empty());
        assert!((s.ledger.positions[&sym()].size - 1.2).abs() < 1e-9);
    }

    /// **部分成交后撤单**：终态 Cancelled 带原始委托量。
    ///
    /// 累计成交量不在挂单回报里（见 `crate::domain::OrderUpdate` 的文档）—— 成交明细由
    /// Fill 流承载，账本由柜台自己维护，回报只负责"这张单结束了"。
    #[test]
    fn cancel_after_partial_fill_reports_original_quantity() {
        let mut s = empty();
        let mut o = limit(Side::Long, 100.0, TimeInForce::GTC, "b1");
        o.quantity = 0.7;
        s.on_order_arrived(1, &o, &"1".to_string());
        // print 0.3 部分成交，剩 0.4 在簿
        s.on_market(&trade_ev_qty(99.0, 0.3, 2));
        let evs = s.on_cancel_arrived(3, &"1".to_string());
        let cancelled: Vec<f64> = evs
            .iter()
            .filter_map(|e| match &e.data {
                ExchangeEventData::OrderUpdate(u) if u.status == OrderStatus::Cancelled => {
                    Some(u.quantity)
                }
                _ => None,
            })
            .collect();
        assert_eq!(cancelled.len(), 1);
        near(cancelled[0], 0.7); // 原始委托量
        // 已成交的 0.3 体现在账本持仓上，而不是撤单回报里
        near(s.ledger.positions[&sym()].size, 0.3);
        assert!(s.resting.is_empty());
    }

    // ===== 市场规则校验（与实盘出口同一规则出处 checked_exchange_qty） =====

    /// **实盘必拒的单，柜台也拒**：不足一个 size_step 的量拒单（Rejected 终态），绝不入簿。
    /// 此前回测路径没有这道校验 —— 小额残单回测照常成交、实盘被拒，参数上线即漂移。
    #[test]
    fn undersized_order_is_rejected_not_matched() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(100.0, 100.1, 1)));
        let mut o = limit(Side::Long, 99.0, TimeInForce::GTC, "b1");
        o.quantity = 0.0004; // < size_step 0.001
        let evs = s.on_order_arrived(1, &o, &"1".to_string());
        assert!(
            statuses(&evs).iter().any(|st| matches!(st, OrderStatus::Rejected { .. })),
            "低于下界必须拒单: {:?}",
            statuses(&evs)
        );
        assert!(s.resting.is_empty(), "未通过校验的单不得入簿");
        // 即使成交穿价也不得成交
        let evs = s.on_market(&trade_ev(98.0, 2));
        assert!(fills(&evs).is_empty());
    }

    /// 缺 SymbolMeta 拒单：真实交易所不存在"没有元数据的合约"，缺失属装配错误
    #[test]
    fn missing_symbol_meta_is_rejected() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(100.0, 100.1, 1)));
        let mut o = limit(Side::Long, 99.0, TimeInForce::GTC, "b1");
        o.symbol = "UNKNOWN".to_string();
        // 行情键按 symbol，先喂一条该 symbol 的行情以免"无参考价"干扰判定
        let evs = s.on_order_arrived(1, &o, &"1".to_string());
        assert!(
            statuses(&evs)
                .iter()
                .any(|st| matches!(st, OrderStatus::Rejected { reason } if reason.contains("SymbolMeta"))),
            "缺元数据必须拒单并指明原因: {:?}",
            statuses(&evs)
        );
    }

    /// 数量按交易所精度**向下取整**后撮合：策略请求 0.0016 而 step=0.001 时，
    /// 实盘只会成交 0.001，柜台必须同量
    #[test]
    fn quantity_is_rounded_down_before_matching() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(100.0, 100.1, 1)));
        let mut o = limit(Side::Long, 100.5, TimeInForce::GTC, "b1"); // 穿价 taker
        o.quantity = 0.0016;
        let evs = s.on_order_arrived(1, &o, &"1".to_string());
        assert_eq!(
            fills(&evs).iter().map(|f| f.size).collect::<Vec<_>>(),
            vec![0.001],
            "应按取整后的量成交"
        );
    }

    /// 喂错所的订单是路由 bug：拒单，不按本所行情撮合
    #[test]
    fn order_for_another_exchange_is_rejected() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(100.0, 100.1, 1)));
        let mut o = limit(Side::Long, 200.0, TimeInForce::GTC, "b1"); // 深度穿价
        o.exchange = Exchange::OKX; // 但属于别的所
        let evs = s.on_order_arrived(1, &o, &"1".to_string());
        assert!(
            statuses(&evs).iter().any(|st| matches!(st, OrderStatus::Rejected { .. })),
            "错所订单必须被拒: {:?}",
            statuses(&evs)
        );
        assert!(fills(&evs).is_empty(), "绝不能按本所行情撮合别的所的订单");
    }

    // ===== trade-native 行情下的可成交性（参考价退化用最新成交价） =====

    /// 市价单在只有成交流的行情下按最新成交价成交，不再被"无盘口"误拒
    #[test]
    fn market_order_fills_at_last_trade_without_bbo() {
        let mut s = empty();
        s.on_market(&trade_ev(100.0, 1));
        let mut o = limit(Side::Long, 0.0, TimeInForce::GTC, "m1");
        o.order_type = OrderType::Market;
        let evs = s.on_order_arrived(2, &o, &"1".to_string());
        assert_eq!(fills(&evs).iter().map(|f| f.price).collect::<Vec<_>>(), vec![100.0]);
    }

    /// **PostOnly 穿价即拒**（trade-native）：限价触及最新成交价就会吃单，真实交易所会拒。
    /// 此前无盘口时 PostOnly 永不被拒 —— 穿价挂单照挂、下一笔 print 还按 maker 费成交，
    /// 系统性放大 maker 策略的回测收益。
    #[test]
    fn postonly_crossing_last_trade_is_rejected() {
        let mut s = empty();
        s.on_market(&trade_ev(100.0, 1));
        let evs = s.on_order_arrived(2, &limit(Side::Long, 100.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        assert!(
            statuses(&evs).iter().any(|st| matches!(st, OrderStatus::Rejected { .. })),
            "买单限价触及最新成交价，PostOnly 必须被拒: {:?}",
            statuses(&evs)
        );
        assert!(s.resting.is_empty());
    }

    /// 深度穿价 GTC（trade-native）到达即按参考价 taker 成交，而不是挂进簿里等
    /// 下一笔 print 按 limit 价收 maker 费（价格与流动性角色双错）
    #[test]
    fn deep_crossing_gtc_fills_as_taker_at_reference_price() {
        let mut s = SimState::empty(EX, 10_000.0, 0.0, 0.002, test_metas());
        s.on_market(&trade_ev(100.0, 1));
        // 买单限价 110 远高于市价 100：应立即以 100 成交、按 taker 计费
        let evs = s.on_order_arrived(2, &limit(Side::Long, 110.0, TimeInForce::GTC, "b1"), &"1".to_string());
        assert_eq!(fills(&evs).iter().map(|f| f.price).collect::<Vec<_>>(), vec![100.0]);
        assert!(s.resting.is_empty(), "穿价单不该 resting");
        near(s.ledger.cash, 10_000.0 - 100.0 * 0.002 * 0.002); // taker 费率
    }

    /// 回报时间戳用**到达时刻**：此前取"最后一条 BBO 的 ts"，行情停推时订单回报的
    /// 时间被冻在过去，订单超时判定随之失效
    #[test]
    fn order_reports_are_stamped_with_arrival_time_not_stale_quote_time() {
        let mut s = empty();
        s.on_market(&market_ev(bbo(50000.0, 50001.0, 1))); // 行情停在 ts=1
        let evs = s.on_order_arrived(999, &limit(Side::Long, 49995.0, TimeInForce::PostOnly, "b1"), &"1".to_string());
        let ts: Vec<u64> = evs.iter().map(|e| e.local_ts).collect();
        assert_eq!(ts, vec![999], "回报时间被冻在旧行情的 ts 上: {ts:?}");
    }

    // ===== 手续费 (对应 SimStateFeeSpec) =====
    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b} got {a}");
    }

    #[test]
    fn taker_fee_charged() {
        let mut s = SimState::empty(EX, 10_000.0, 0.001, 0.002, test_metas());
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
        let mut s = SimState::empty(EX, 10_000.0, 0.001, 0.002, test_metas());
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

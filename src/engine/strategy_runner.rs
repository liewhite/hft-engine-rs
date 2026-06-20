//! StrategyRunner —— 策略执行的**纯逻辑核心**。
//!
//! 把一个事件喂给策略、维护策略独享状态、产出"已按交易所精度转换"的信号。不含任何
//! 传输/并发设施 (无 actor / channel / pubsub)，因此被两种驱动复用：
//!   - 实盘 [`crate::engine::ExecutorActor`]：actor 串行消费总线事件，调用 [`StrategyRunner::on_event`]
//!     后把结果发布到 OutcomePubSub
//!   - 回测 [`crate::backtest::BacktestEngine`]：单线程虚拟时间循环里同步调用 [`StrategyRunner::accepts`] /
//!     [`StrategyRunner::on_event`]
//!
//! 职责：过滤订阅范围、更新 StateManager、分配 client_order_id、登记 pending、按 SymbolMeta
//! 转换 (币本位->张数、价格/数量取整)。

use crate::domain::{Exchange, Order, OrderType, Symbol, SymbolMeta};
use crate::messaging::{IncomeEvent, StateManager};
use crate::strategy::{OutcomeEvent, Strategy};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// client_order_id 生成策略 (注入点)。
///
/// 生产用交易所规范的随机 id (UUID) 保证跨重启唯一；回测用确定性自增计数，保证
/// "同一输入 -> 同一逐笔回报序列" (client_order_id 会随订单/成交回报投递给策略)。
pub trait ClientOrderIdGen: Send {
    fn next_id(&mut self, exchange: Exchange) -> String;
}

/// 生产默认：交易所规范的随机 client_order_id (UUID)。
#[derive(Default)]
pub struct ExchangeUuidGen;
impl ClientOrderIdGen for ExchangeUuidGen {
    fn next_id(&mut self, exchange: Exchange) -> String {
        exchange.new_cli_order_id()
    }
}

/// 回测确定性：自增计数 (与墙钟/随机无关)。
#[derive(Default)]
pub struct SequentialClientOrderIdGen {
    next: u64,
}
impl ClientOrderIdGen for SequentialClientOrderIdGen {
    fn next_id(&mut self, _exchange: Exchange) -> String {
        let id = format!("bt{}", self.next);
        self.next += 1;
        id
    }
}

pub struct StrategyRunner {
    strategy: Box<dyn Strategy>,
    state: StateManager,
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 策略订阅的 (exchange, symbol) 集合，用于事件过滤
    subscriptions: HashSet<(Exchange, Symbol)>,
    /// client_order_id 生成策略 (生产 UUID / 回测确定性计数)
    id_gen: Box<dyn ClientOrderIdGen>,
}

impl StrategyRunner {
    /// 生产构造：client_order_id 用交易所 UUID。
    pub fn new(
        strategy: Box<dyn Strategy>,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    ) -> Self {
        Self::with_id_gen(strategy, symbol_metas, Box::new(ExchangeUuidGen))
    }

    /// 注入 id 生成策略 (回测传 [`SequentialClientOrderIdGen`] 以确保确定性)。
    pub fn with_id_gen(
        strategy: Box<dyn Strategy>,
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        id_gen: Box<dyn ClientOrderIdGen>,
    ) -> Self {
        // 从 public_streams 推导订阅的 (exchange, symbol) 集合
        let subscriptions: HashSet<(Exchange, Symbol)> = strategy
            .public_streams()
            .iter()
            .flat_map(|(ex, kinds)| kinds.iter().map(move |k| (*ex, k.symbol().clone())))
            .collect();
        let symbols: Vec<Symbol> = subscriptions.iter().map(|(_, s)| s.clone()).collect();
        let state = StateManager::new(&symbols, strategy.order_timeout_ms());
        Self {
            strategy,
            state,
            symbol_metas,
            subscriptions,
            id_gen,
        }
    }

    /// 只读访问策略状态 (供观察/调试)。
    pub fn state(&self) -> &StateManager {
        &self.state
    }

    /// 全局事件 (无路由键) 广播；symbol 事件仅接收订阅范围内的。
    pub fn accepts(&self, event: &IncomeEvent) -> bool {
        match event.routing() {
            Some(route) => self.subscriptions.contains(&route),
            None => true,
        }
    }

    /// 更新状态并运行策略，返回**已转换为交易所格式**的信号 (下单已分配 id、登记 pending、取整)。
    pub fn on_event(&mut self, event: &IncomeEvent) -> Vec<OutcomeEvent> {
        self.state.apply(event);
        self.strategy
            .on_event(event, &self.state)
            .into_iter()
            .map(|signal| match signal {
                OutcomeEvent::PlaceOrders { orders, comment } => {
                    let converted = orders
                        .into_iter()
                        .map(|mut order| {
                            order.client_order_id = self.id_gen.next_id(order.exchange);
                            // 原始订单 (币本位) 登记到 pending，策略端统一看到币的数量；
                            // created_at 取当前事件时刻 (回测=虚拟时间, 确定性), 不读墙钟
                            self.state.add_pending_order(order.clone(), event.local_ts);
                            // 转换为交易所格式 (合约张数 + 价格/数量取整)
                            self.convert_order(order)
                        })
                        .collect();
                    OutcomeEvent::PlaceOrders {
                        orders: converted,
                        comment,
                    }
                }
                cancel @ OutcomeEvent::CancelOrder { .. } => cancel,
            })
            .collect()
    }

    /// 币本位数量 -> 合约张数，价格/数量按交易所精度取整。
    /// 缺少 SymbolMeta 说明策略交易了未预加载的 symbol，仅记录告警并按原值发出 (与实盘行为一致)。
    fn convert_order(&self, order: Order) -> Order {
        let key = (order.exchange, order.symbol.clone());
        let meta = match self.symbol_metas.get(&key) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    "SymbolMeta not found, order not converted"
                );
                return order;
            }
        };
        let quantity = meta.round_size_down(meta.coin_to_qty(order.quantity));
        let order_type = match &order.order_type {
            OrderType::Market => OrderType::Market,
            OrderType::Limit { price, tif } => OrderType::Limit {
                price: meta.round_price(*price),
                tif: *tif,
            },
        };
        Order {
            order_type,
            quantity,
            ..order
        }
    }
}

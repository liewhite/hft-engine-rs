use crate::domain::{now_ms, Exchange, Order, OrderType, Side, Symbol, SymbolMeta, USDT};
use crate::engine::SignalProcessorActor;
use crate::messaging::{ExchangeEvent, SymbolState};
use crate::strategy::Signal;
use kameo::actor::ActorRef;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// 状态管理器 - 管理所有交易状态并提供下单接口
pub struct StateManager {
    /// Per-symbol 状态
    states: HashMap<Symbol, SymbolState>,
    /// 全局 USDT 余额 (per exchange)
    balances: HashMap<Exchange, f64>,
    /// 账户净值 (per exchange)
    equities: HashMap<Exchange, f64>,
    /// Symbol 元数据 (用于订单转换)
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// SignalProcessorActor 引用
    signal_processor: ActorRef<SignalProcessorActor>,
    /// 订单超时时间 (毫秒)
    order_timeout_ms: u64,
}

impl StateManager {
    /// 创建状态管理器
    pub fn new(
        symbols: &[Symbol],
        symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
        signal_processor: ActorRef<SignalProcessorActor>,
        order_timeout_ms: u64,
    ) -> Self {
        let mut states = HashMap::new();
        for symbol in symbols {
            states.insert(symbol.clone(), SymbolState::new(symbol.clone()));
        }

        Self {
            states,
            balances: HashMap::new(),
            equities: HashMap::new(),
            symbol_metas,
            signal_processor,
            order_timeout_ms,
        }
    }

    // ==================== 下单接口 ====================

    /// 下单：生成 client_order_id，转换订单，添加到 pending_orders，发送到 SignalProcessor
    pub fn place_order(&mut self, mut order: Order) {
        // 生成 client_order_id (去掉 `-`，OKX 只允许字母数字)
        let client_order_id = Uuid::new_v4().simple().to_string();
        order.client_order_id = Some(client_order_id.clone());

        // 转换订单 (coin -> qty, round price/size)
        let converted_order = self.convert_order(&order);

        // 添加到对应 symbol 的 pending_orders
        if let Some(state) = self.states.get_mut(&order.symbol) {
            state.add_pending_order(client_order_id, order.exchange, now_ms());
        }

        // 发送到 SignalProcessorActor
        let signal_processor = self.signal_processor.clone();
        tokio::spawn(async move {
            let _ = signal_processor.tell(Signal::PlaceOrder(converted_order)).await;
        });
    }

    /// 批量下单
    pub fn place_orders(&mut self, orders: Vec<Order>) {
        for order in orders {
            self.place_order(order);
        }
    }

    /// 转换订单：coin_to_qty + round price/size
    fn convert_order(&self, order: &Order) -> Order {
        let key = (order.exchange, order.symbol.clone());
        let meta = match self.symbol_metas.get(&key) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    exchange = %order.exchange,
                    symbol = %order.symbol,
                    "SymbolMeta not found, order not converted"
                );
                return order.clone();
            }
        };

        // 转换并取整 quantity
        let qty_in_contracts = meta.coin_to_qty(order.quantity);
        let rounded_qty = meta.round_size_down(qty_in_contracts);

        // 转换 price (仅 Limit 订单)
        let converted_order_type = match &order.order_type {
            OrderType::Market => OrderType::Market,
            OrderType::Limit { price, tif } => {
                let rounded_price = match order.side {
                    Side::Long => meta.round_price_up(*price),
                    Side::Short => meta.round_price_down(*price),
                };
                OrderType::Limit {
                    price: rounded_price,
                    tif: *tif,
                }
            }
        };

        Order {
            id: order.id.clone(),
            exchange: order.exchange,
            symbol: order.symbol.clone(),
            side: order.side,
            order_type: converted_order_type,
            quantity: rounded_qty,
            reduce_only: order.reduce_only,
            client_order_id: order.client_order_id.clone(),
        }
    }

    // ==================== 状态查询 ====================

    /// 获取指定 symbol 的状态
    pub fn symbol_state(&self, symbol: &Symbol) -> Option<&SymbolState> {
        self.states.get(symbol)
    }

    /// 获取指定交易所的 USDT 余额
    pub fn usdt_balance(&self, exchange: Exchange) -> f64 {
        self.balances.get(&exchange).copied().unwrap_or(0.0)
    }

    /// 获取所有交易所的 USDT 总余额
    pub fn total_usdt_balance(&self) -> f64 {
        self.balances.values().sum()
    }

    /// 获取指定交易所的账户净值
    pub fn equity(&self, exchange: Exchange) -> f64 {
        self.equities.get(&exchange).copied().unwrap_or(0.0)
    }

    /// 获取所有交易所的总净值
    pub fn total_equity(&self) -> f64 {
        self.equities.values().sum()
    }

    /// 检查指定 symbol 是否有未完成订单
    pub fn has_pending_orders(&self, symbol: &Symbol) -> bool {
        self.states
            .get(symbol)
            .map(|s| s.has_pending_orders())
            .unwrap_or(false)
    }

    // ==================== 事件处理 ====================

    /// 处理事件，更新状态
    pub fn apply(&mut self, event: &ExchangeEvent) {
        match event {
            // 全局事件: Balance
            ExchangeEvent::BalanceUpdate { exchange, balance, .. } => {
                if balance.asset == USDT {
                    tracing::debug!(
                        exchange = %exchange,
                        available = balance.available,
                        "USDT balance updated"
                    );
                    self.balances.insert(*exchange, balance.available);
                }
            }
            // 全局事件: Equity
            ExchangeEvent::EquityUpdate { exchange, equity, .. } => {
                tracing::debug!(
                    exchange = %exchange,
                    equity = equity,
                    "Equity updated"
                );
                self.equities.insert(*exchange, *equity);
            }
            // 全局事件: Clock (检查订单超时)
            ExchangeEvent::Clock { timestamp } => {
                for state in self.states.values_mut() {
                    state.remove_timed_out_orders(*timestamp, self.order_timeout_ms);
                }
            }
            // Symbol 事件: 委托对应 SymbolState 处理
            _ => {
                if let Some(symbol) = event.symbol() {
                    if let Some(state) = self.states.get_mut(symbol) {
                        state.apply(event);
                    }
                }
            }
        }
    }
}

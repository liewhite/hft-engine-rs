# 回测引擎设计文档

## 1. 设计目标

构建一个与实盘架构高度一致的回测引擎，实现：
- **精确性优先**：完全确定性的事件驱动，保证回测结果可复现
- **延迟仿真**：通过双事件队列模拟网络延迟，还原真实交易环境
- **架构复用**：复用现有数据结构（IncomeEvent, Order, OrderUpdate等）和Strategy trait
- **可扩展数据源**：支持多种历史数据来源（CSV、数据库、Parquet等）

## 2. 核心设计：双事件队列 + 延迟模型

### 2.1 为什么需要双队列？

实盘中存在网络延迟：
- **交易所 → 本地**：行情推送、订单状态更新有延迟
- **本地 → 交易所**：下单请求有延迟

单队列无法正确模拟这种延迟场景。例如：
```
时间T: 交易所价格变为100
时间T+5ms: 本地收到价格100
时间T+3ms: 本地下单（基于旧价格99）
时间T+8ms: 交易所收到下单请求
```

如果只用单队列，下单时策略已经"看到"了100的价格，这是不正确的。

### 2.2 双队列架构

```
┌─────────────────────────────────────────────────────────────────┐
│                    BacktestProcessor                            │
│                                                                 │
│  ┌─────────────────────┐      ┌─────────────────────┐          │
│  │   Exchange Queue    │      │    Local Queue      │          │
│  │   (交易所事件队列)   │      │   (本地事件队列)    │          │
│  │                     │      │                     │          │
│  │ - 原始行情事件      │      │ - 延迟后的行情      │          │
│  │ - 下单请求(+延迟)   │      │ - 延迟后的订单状态  │          │
│  │ - 撮合触发         │      │ - Clock事件         │          │
│  └──────────┬──────────┘      └──────────┬──────────┘          │
│             │                            │                      │
│             └────────────┬───────────────┘                      │
│                          ▼                                      │
│                 取两队列中最早时间戳                             │
│                          │                                      │
│             ┌────────────┴────────────┐                        │
│             ▼                         ▼                        │
│     Exchange Event              Local Event                    │
│     (同步处理)                   (同步处理)                     │
│             │                         │                        │
│             ▼                         ▼                        │
│     ┌───────────────┐         ┌───────────────┐               │
│     │  OrderBook    │         │   Strategy    │               │
│     │  (撮合引擎)   │         │   Executor    │               │
│     └───────────────┘         └───────────────┘               │
└─────────────────────────────────────────────────────────────────┘
```

### 2.3 事件流转与延迟

```
数据源产生行情 (时间 T)
        │
        ▼
   Exchange Queue ──────────────────┐
        │                           │
        │ 立即处理                   │ 加延迟后插入
        ▼                           ▼
   OrderBook 更新              Local Queue (时间 T+delay)
   检查限价单撮合                    │
        │                           │
        │ 撮合产生OrderUpdate        │ 策略看到行情
        │ (加延迟后)                 │
        ▼                           ▼
   Local Queue ◄──────────────  Strategy.on_event()
   (时间 T+delay)                   │
                                    │ 策略下单
                                    ▼
                              Local Queue (下单事件)
                                    │
                                    │ 加延迟后插入
                                    ▼
                              Exchange Queue (时间 T'+delay)
                                    │
                                    ▼
                              OrderBook 收到订单
                              立即撮合(市价单)或挂单(限价单)
```

### 2.4 精确性保证：同步单线程处理

**关键约束**：为保证事件处理的确定性，核心逻辑必须**同步单线程**执行。

**问题场景**（如果使用异步 Actor 消息）：
```
1. Processor 收到事件 A (时间戳 100ms)
2. Processor 发送消息给 OrderBook Actor
3. 在等待 OrderBook 响应时，Processor 又收到事件 B (时间戳 95ms)
4. 事件 B 可能先被处理，导致时序错乱
```

**解决方案**：
- OrderBook 和 StrategyExecutor 作为**普通 struct**，由 Processor **同步调用**
- 每个事件完全处理完毕（包括所有副作用）后，才处理下一个事件
- Actor 模式仅用于外层（Manager 管理生命周期、DataSource 异步加载数据）

```rust
// 正确：同步调用
fn process_event(&mut self, event: TimedEvent) {
    match event.queue_type {
        QueueType::Exchange => {
            // 同步调用 OrderBook
            let updates = self.order_book.on_exchange_event(&event);
            // 立即处理产生的事件
            for update in updates {
                self.local_queue.push(update.with_delay(self.config.delay));
            }
        }
        QueueType::Local => {
            // 同步调用 Strategy
            let orders = self.executor.on_local_event(&event);
            // 立即处理产生的下单
            for order in orders {
                self.exchange_queue.push(order.with_delay(self.config.delay));
            }
        }
    }
}
```

## 3. 架构概览

### 3.1 组件结构

```
BacktestManager (入口，可选 Actor)
│
├── DataSource (异步加载数据)
│   └── 预加载所有事件到 Exchange Queue
│
└── BacktestProcessor (核心引擎，同步执行)
    ├── exchange_queue: BinaryHeap<ExchangeEvent>
    ├── local_queue: BinaryHeap<LocalEvent>
    ├── order_book: OrderBook (同步调用)
    ├── executors: Vec<StrategyExecutor> (同步调用)
    └── state_manager: StateManager
```

### 3.2 与实盘架构对比

| 实盘组件 | 回测对应 | 说明 |
|---------|---------|------|
| ManagerActor | BacktestManager | 顶层协调 |
| ProcessorActor | BacktestProcessor | 双队列 + 同步事件循环 |
| ExecutorActor | StrategyExecutor (struct) | 同步调用，非 Actor |
| SignalProcessorActor | OrderBook (struct) | 同步撮合，非 Actor |
| ClockActor | 集成到 local_queue | Clock 事件预生成 |
| ExchangeActor | DataSource | 预加载数据到 exchange_queue |

## 4. 核心组件设计

### 4.1 事件类型定义

```rust
/// 交易所侧事件
#[derive(Clone)]
pub enum ExchangeEvent {
    /// 行情更新（来自数据源）
    MarketData(IncomeEvent),
    /// 收到下单请求（来自本地，经过延迟）
    OrderRequest {
        order: Order,
        request_time: Timestamp,  // 本地发出时间
    },
}

/// 本地侧事件
#[derive(Clone)]
pub enum LocalEvent {
    /// 收到行情（来自交易所，经过延迟）
    MarketData(IncomeEvent),
    /// 收到订单状态更新（来自交易所，经过延迟）
    OrderUpdate(OrderUpdate),
    /// 时钟事件
    Clock(Timestamp),
}

/// 带时间戳的事件包装
pub struct TimedEvent<E> {
    pub timestamp: Timestamp,
    pub sequence: u64,  // 同一时间戳的事件序号，保证稳定排序
    pub event: E,
}
```

### 4.2 BacktestProcessor（核心引擎）

**职责**：
1. 维护双事件队列，保证时序正确
2. 同步处理每个事件，调用 OrderBook 和 StrategyExecutor
3. 管理延迟模型，在队列间传递事件

```rust
pub struct BacktestProcessor {
    /// 交易所事件队列（最小堆）
    exchange_queue: BinaryHeap<Reverse<TimedEvent<ExchangeEvent>>>,

    /// 本地事件队列（最小堆）
    local_queue: BinaryHeap<Reverse<TimedEvent<LocalEvent>>>,

    /// 事件序号计数器（保证同时间戳事件的稳定排序）
    sequence_counter: u64,

    /// 当前仿真时间
    current_time: Timestamp,

    /// 订单簿（同步调用）
    order_book: OrderBook,

    /// 策略执行器（同步调用）
    executors: Vec<StrategyExecutor>,

    /// 延迟配置
    latency_config: LatencyConfig,

    /// 回测结果收集器
    result_collector: ResultCollector,
}

/// 延迟配置
pub struct LatencyConfig {
    /// 交易所 → 本地 延迟（行情、订单状态）
    pub exchange_to_local_ms: u64,
    /// 本地 → 交易所 延迟（下单请求）
    pub local_to_exchange_ms: u64,
}
```

**主循环**：

```rust
impl BacktestProcessor {
    /// 运行回测主循环
    pub fn run(&mut self) -> BacktestResult {
        while !self.is_finished() {
            // 取两个队列中时间戳最小的事件
            let next_event = self.peek_next_event();

            match next_event {
                NextEvent::Exchange(ts) => {
                    let event = self.exchange_queue.pop().unwrap().0;
                    self.current_time = event.timestamp;
                    self.process_exchange_event(event.event);
                }
                NextEvent::Local(ts) => {
                    let event = self.local_queue.pop().unwrap().0;
                    self.current_time = event.timestamp;
                    self.process_local_event(event.event);
                }
                NextEvent::Empty => break,
            }
        }

        self.result_collector.finalize()
    }

    fn peek_next_event(&self) -> NextEvent {
        match (self.exchange_queue.peek(), self.local_queue.peek()) {
            (Some(e), Some(l)) => {
                if e.0.timestamp <= l.0.timestamp {
                    NextEvent::Exchange(e.0.timestamp)
                } else {
                    NextEvent::Local(l.0.timestamp)
                }
            }
            (Some(e), None) => NextEvent::Exchange(e.0.timestamp),
            (None, Some(l)) => NextEvent::Local(l.0.timestamp),
            (None, None) => NextEvent::Empty,
        }
    }
}
```

**事件处理**：

```rust
impl BacktestProcessor {
    /// 处理交易所侧事件
    fn process_exchange_event(&mut self, event: ExchangeEvent) {
        match event {
            ExchangeEvent::MarketData(income_event) => {
                // 1. 更新 OrderBook
                let order_updates = self.order_book.on_market_data(&income_event);

                // 2. 撮合产生的 OrderUpdate 加延迟后送入本地队列
                for update in order_updates {
                    self.push_local_event(
                        LocalEvent::OrderUpdate(update),
                        self.current_time + self.latency_config.exchange_to_local_ms,
                    );
                }

                // 3. 行情事件加延迟后送入本地队列（策略可见）
                self.push_local_event(
                    LocalEvent::MarketData(income_event),
                    self.current_time + self.latency_config.exchange_to_local_ms,
                );
            }

            ExchangeEvent::OrderRequest { order, request_time } => {
                // 下单请求到达交易所，立即处理
                let order_updates = self.order_book.on_order_request(order, self.current_time);

                // OrderUpdate 加延迟后送入本地队列
                for update in order_updates {
                    self.push_local_event(
                        LocalEvent::OrderUpdate(update),
                        self.current_time + self.latency_config.exchange_to_local_ms,
                    );
                }
            }
        }
    }

    /// 处理本地侧事件
    fn process_local_event(&mut self, event: LocalEvent) {
        match event {
            LocalEvent::MarketData(income_event) | LocalEvent::OrderUpdate(_) | LocalEvent::Clock(_) => {
                // 转换为统一的 IncomeEvent 格式
                let income_event = self.to_income_event(&event);

                // 同步调用每个策略执行器
                for executor in &mut self.executors {
                    let orders = executor.on_event(&income_event, self.current_time);

                    // 策略产生的下单请求加延迟后送入交易所队列
                    for order in orders {
                        self.push_exchange_event(
                            ExchangeEvent::OrderRequest {
                                order,
                                request_time: self.current_time,
                            },
                            self.current_time + self.latency_config.local_to_exchange_ms,
                        );
                    }
                }
            }
        }
    }

    fn push_exchange_event(&mut self, event: ExchangeEvent, timestamp: Timestamp) {
        self.sequence_counter += 1;
        self.exchange_queue.push(Reverse(TimedEvent {
            timestamp,
            sequence: self.sequence_counter,
            event,
        }));
    }

    fn push_local_event(&mut self, event: LocalEvent, timestamp: Timestamp) {
        self.sequence_counter += 1;
        self.local_queue.push(Reverse(TimedEvent {
            timestamp,
            sequence: self.sequence_counter,
            event,
        }));
    }
}
```

### 4.3 OrderBook（订单簿模拟）

**职责**：
1. 维护每个交易对的最新 BBO
2. 管理挂单（限价单）
3. 执行撮合逻辑
4. 返回 OrderUpdate 列表

```rust
pub struct OrderBook {
    /// 每个交易对的最新 BBO
    bbos: HashMap<(Exchange, Symbol), BBO>,

    /// 挂单列表：按 (exchange, symbol) 分组
    pending_orders: HashMap<(Exchange, Symbol), Vec<PendingOrder>>,

    /// 手续费配置
    fee_config: FeeConfig,
}

struct PendingOrder {
    order: Order,
    submit_time: Timestamp,
}

impl OrderBook {
    /// 处理行情更新，返回被触发的订单更新
    pub fn on_market_data(&mut self, event: &IncomeEvent) -> Vec<OrderUpdate> {
        let mut updates = Vec::new();

        if let ExchangeEventData::BBO(bbo) = &event.data {
            let key = (bbo.exchange.clone(), bbo.symbol.clone());

            // 更新 BBO
            self.bbos.insert(key.clone(), bbo.clone());

            // 检查挂单是否可成交
            if let Some(pending) = self.pending_orders.get_mut(&key) {
                let mut filled_indices = Vec::new();

                for (i, p) in pending.iter().enumerate() {
                    if let Some(update) = self.try_fill_order(&p.order, bbo, event.exchange_ts) {
                        updates.push(update);
                        filled_indices.push(i);
                    }
                }

                // 移除已成交的订单（逆序删除避免索引错乱）
                for i in filled_indices.into_iter().rev() {
                    pending.remove(i);
                }
            }
        }

        updates
    }

    /// 处理下单请求，返回订单状态更新
    pub fn on_order_request(&mut self, order: Order, timestamp: Timestamp) -> Vec<OrderUpdate> {
        let key = (order.exchange.clone(), order.symbol.clone());
        let bbo = self.bbos.get(&key);

        match &order.order_type {
            OrderType::Market => {
                // 市价单立即成交
                if let Some(bbo) = bbo {
                    vec![self.fill_market_order(&order, bbo, timestamp)]
                } else {
                    // 无行情，拒绝订单
                    vec![OrderUpdate {
                        order_id: order.id.clone(),
                        client_order_id: Some(order.client_order_id.clone()),
                        exchange: order.exchange.clone(),
                        symbol: order.symbol.clone(),
                        status: OrderStatus::Rejected {
                            reason: "No market data".to_string(),
                        },
                        filled_quantity: 0.0,
                        avg_price: None,
                        timestamp,
                    }]
                }
            }
            OrderType::Limit { price, .. } => {
                // 限价单：检查是否可立即成交
                if let Some(bbo) = bbo {
                    if self.can_fill_limit_order(&order, *price, bbo) {
                        vec![self.fill_limit_order(&order, *price, bbo, timestamp)]
                    } else {
                        // 挂单
                        self.pending_orders
                            .entry(key)
                            .or_default()
                            .push(PendingOrder {
                                order: order.clone(),
                                submit_time: timestamp,
                            });

                        vec![OrderUpdate {
                            order_id: order.id.clone(),
                            client_order_id: Some(order.client_order_id.clone()),
                            exchange: order.exchange.clone(),
                            symbol: order.symbol.clone(),
                            status: OrderStatus::Pending,
                            filled_quantity: 0.0,
                            avg_price: None,
                            timestamp,
                        }]
                    }
                } else {
                    vec![OrderUpdate {
                        order_id: order.id.clone(),
                        client_order_id: Some(order.client_order_id.clone()),
                        exchange: order.exchange.clone(),
                        symbol: order.symbol.clone(),
                        status: OrderStatus::Rejected {
                            reason: "No market data".to_string(),
                        },
                        filled_quantity: 0.0,
                        avg_price: None,
                        timestamp,
                    }]
                }
            }
        }
    }

    fn fill_market_order(&self, order: &Order, bbo: &BBO, timestamp: Timestamp) -> OrderUpdate {
        let fill_price = match order.side {
            Side::Long => bbo.ask_price,
            Side::Short => bbo.bid_price,
        };

        OrderUpdate {
            order_id: order.id.clone(),
            client_order_id: Some(order.client_order_id.clone()),
            exchange: order.exchange.clone(),
            symbol: order.symbol.clone(),
            status: OrderStatus::Filled,
            filled_quantity: order.quantity,
            avg_price: Some(fill_price),
            timestamp,
        }
    }

    fn can_fill_limit_order(&self, order: &Order, limit_price: Price, bbo: &BBO) -> bool {
        match order.side {
            Side::Long => limit_price >= bbo.ask_price,
            Side::Short => limit_price <= bbo.bid_price,
        }
    }

    fn fill_limit_order(&self, order: &Order, limit_price: Price, bbo: &BBO, timestamp: Timestamp) -> OrderUpdate {
        // 限价单成交价格：取限价和对手价的更优者
        let fill_price = match order.side {
            Side::Long => limit_price.min(bbo.ask_price),
            Side::Short => limit_price.max(bbo.bid_price),
        };

        OrderUpdate {
            order_id: order.id.clone(),
            client_order_id: Some(order.client_order_id.clone()),
            exchange: order.exchange.clone(),
            symbol: order.symbol.clone(),
            status: OrderStatus::Filled,
            filled_quantity: order.quantity,
            avg_price: Some(fill_price),
            timestamp,
        }
    }

    fn try_fill_order(&self, order: &Order, bbo: &BBO, timestamp: Timestamp) -> Option<OrderUpdate> {
        if let OrderType::Limit { price, .. } = &order.order_type {
            if self.can_fill_limit_order(order, *price, bbo) {
                return Some(self.fill_limit_order(order, *price, bbo, timestamp));
            }
        }
        None
    }
}
```

### 4.4 StrategyExecutor（策略执行器）

**职责**：
1. 包装 Strategy 实例
2. 维护策略专属的 StateManager
3. 同步调用 Strategy.on_event()
4. 返回策略产生的下单请求

```rust
pub struct StrategyExecutor {
    /// 策略实例
    strategy: Box<dyn Strategy>,

    /// 状态管理器
    state_manager: StateManager,

    /// 订阅的 symbol 列表
    subscribed_symbols: HashSet<(Exchange, Symbol)>,
}

impl StrategyExecutor {
    pub fn new(strategy: Box<dyn Strategy>, order_timeout_ms: u64) -> Self {
        let subscribed_symbols = strategy
            .public_streams()
            .into_iter()
            .flat_map(|(exchange, subs)| {
                subs.into_iter().map(move |s| match s {
                    SubscriptionKind::BBO { symbol } => (exchange.clone(), symbol),
                    SubscriptionKind::FundingRate { symbol } => (exchange.clone(), symbol),
                })
            })
            .collect();

        Self {
            strategy,
            state_manager: StateManager::new(order_timeout_ms),
            subscribed_symbols,
        }
    }

    /// 处理事件，返回下单请求列表
    pub fn on_event(&mut self, event: &IncomeEvent, current_time: Timestamp) -> Vec<Order> {
        // 检查是否订阅了该事件
        if !self.should_process(event) {
            return vec![];
        }

        // 更新状态
        self.state_manager.apply(event);

        // 调用策略
        let outcomes = self.strategy.on_event(event, &self.state_manager);

        // 转换为订单
        outcomes
            .into_iter()
            .filter_map(|outcome| match outcome {
                OutcomeEvent::PlaceOrder(order) => Some(order),
                _ => None,
            })
            .collect()
    }

    fn should_process(&self, event: &IncomeEvent) -> bool {
        match &event.data {
            ExchangeEventData::BBO(bbo) => {
                self.subscribed_symbols.contains(&(bbo.exchange.clone(), bbo.symbol.clone()))
            }
            ExchangeEventData::FundingRate(fr) => {
                self.subscribed_symbols.contains(&(fr.exchange.clone(), fr.symbol.clone()))
            }
            // Clock, Balance, Equity 等全局事件始终处理
            _ => true,
        }
    }
}
```

### 4.5 DataSource Trait

**设计目标**：支持多种数据源，在回测开始前预加载所有数据。

```rust
/// 数据源 trait
pub trait DataSource: Send {
    /// 数据源名称
    fn name(&self) -> &str;

    /// 加载所有事件（同步方法，回测开始前调用）
    fn load_events(&mut self) -> Result<Vec<IncomeEvent>, DataSourceError>;
}

/// CSV 数据源
pub struct CsvDataSource {
    name: String,
    exchange: Exchange,
    symbol: Symbol,
    data_type: DataType,
    file_path: PathBuf,
}

impl DataSource for CsvDataSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn load_events(&mut self) -> Result<Vec<IncomeEvent>, DataSourceError> {
        // 读取 CSV 文件，解析为 IncomeEvent
        todo!()
    }
}

/// 内存数据源（测试用）
pub struct MemoryDataSource {
    name: String,
    events: Vec<IncomeEvent>,
}

impl DataSource for MemoryDataSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn load_events(&mut self) -> Result<Vec<IncomeEvent>, DataSourceError> {
        Ok(std::mem::take(&mut self.events))
    }
}
```

### 4.6 BacktestManager

**职责**：
1. 初始化所有组件
2. 预加载数据到队列
3. 运行回测
4. 返回结果

```rust
pub struct BacktestManager {
    config: BacktestConfig,
    data_sources: Vec<Box<dyn DataSource>>,
    strategies: Vec<Box<dyn Strategy>>,
}

pub struct BacktestConfig {
    /// 回测时间范围
    pub start_time: Timestamp,
    pub end_time: Timestamp,

    /// 延迟配置
    pub latency: LatencyConfig,

    /// 初始资金
    pub initial_capital: HashMap<Exchange, f64>,

    /// 手续费配置
    pub fee_config: FeeConfig,

    /// 时钟间隔（毫秒）
    pub clock_interval_ms: u64,
}

impl BacktestManager {
    pub fn new(config: BacktestConfig) -> Self {
        Self {
            config,
            data_sources: Vec::new(),
            strategies: Vec::new(),
        }
    }

    pub fn add_data_source(&mut self, source: Box<dyn DataSource>) -> &mut Self {
        self.data_sources.push(source);
        self
    }

    pub fn add_strategy(&mut self, strategy: Box<dyn Strategy>) -> &mut Self {
        self.strategies.push(strategy);
        self
    }

    /// 运行回测
    pub fn run(mut self) -> Result<BacktestResult, BacktestError> {
        // 1. 创建 Processor
        let mut processor = BacktestProcessor::new(
            self.config.latency.clone(),
            self.config.fee_config.clone(),
        );

        // 2. 加载数据源，插入交易所队列
        for source in &mut self.data_sources {
            let events = source.load_events()?;
            for event in events {
                if event.exchange_ts >= self.config.start_time
                    && event.exchange_ts <= self.config.end_time
                {
                    processor.push_exchange_event(
                        ExchangeEvent::MarketData(event.clone()),
                        event.exchange_ts,
                    );
                }
            }
        }

        // 3. 生成 Clock 事件
        let mut t = self.config.start_time;
        while t <= self.config.end_time {
            processor.push_local_event(LocalEvent::Clock(t), t);
            t += self.config.clock_interval_ms;
        }

        // 4. 创建策略执行器
        for strategy in self.strategies {
            let executor = StrategyExecutor::new(
                strategy,
                10000, // order_timeout_ms
            );
            processor.add_executor(executor);
        }

        // 5. 运行主循环
        Ok(processor.run())
    }
}

## 5. 与实盘共享组件

### 5.1 完全复用

- `Strategy` trait 和所有策略实现
- `StateManager` 和 `SymbolState`
- 所有 `domain/models` 数据结构
- 所有事件类型 `IncomeEvent`, `ExchangeEventData`

### 5.2 回测专用组件

| 组件 | 实盘 | 回测 | 说明 |
|------|------|------|------|
| Processor | Actor (异步) | struct (同步) | 回测需要确定性 |
| Executor | Actor | struct | 同步调用策略 |
| OrderBook | 无（交易所撮合） | struct | 本地模拟撮合 |
| 数据源 | WebSocket Actor | DataSource trait | 预加载历史数据 |

## 6. 回测结果与统计

### 6.1 统计数据收集

```rust
pub struct BacktestResult {
    /// 时间范围
    pub start_time: Timestamp,
    pub end_time: Timestamp,

    /// 盈亏曲线：每个时间点的净值
    pub equity_curve: Vec<(Timestamp, f64)>,

    /// 交易记录
    pub trades: Vec<TradeRecord>,

    /// 统计指标
    pub metrics: BacktestMetrics,
}

pub struct TradeRecord {
    pub timestamp: Timestamp,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub quantity: Quantity,
    pub price: Price,
    pub fee: f64,
    pub pnl: f64,  // 该笔交易的盈亏
}

pub struct BacktestMetrics {
    pub total_pnl: f64,
    pub total_fee: f64,
    pub win_rate: f64,
    pub max_drawdown: f64,
    pub sharpe_ratio: f64,
    pub total_trades: usize,
    pub avg_holding_time: Duration,
}
```

### 6.2 净值计算

每次处理完一个时间点的所有事件后，计算当前净值：

```rust
fn calculate_equity(&self) -> f64 {
    let mut equity = 0.0;

    // 现金
    for (_, balance) in &self.balances {
        equity += balance;
    }

    // 仓位市值
    for (_, state) in &self.state_manager.states {
        for (exchange, position) in &state.positions {
            let bbo = state.bbos.get(exchange);
            if let Some(bbo) = bbo {
                let mark_price = (bbo.bid_price + bbo.ask_price) / 2.0;
                equity += position.size * mark_price;
            }
        }
    }

    equity
}
```

## 7. 完整数据流图

```
                    ┌─────────────────────────────────────────────────────────┐
                    │                    BacktestProcessor                    │
                    │                                                         │
┌─────────────┐     │  ┌─────────────────────┐  ┌─────────────────────┐      │
│ DataSource  │─────┼─▶│   Exchange Queue    │  │    Local Queue      │      │
│ (预加载)    │     │  │                     │  │                     │      │
└─────────────┘     │  │ T=100: BBO          │  │ T=105: BBO(延迟后)  │      │
                    │  │ T=108: OrderRequest │  │ T=105: Clock        │      │
                    │  │ T=110: BBO          │  │ T=113: OrderUpdate  │      │
                    │  └──────────┬──────────┘  └──────────┬──────────┘      │
                    │             │                        │                  │
                    │             └────────────┬───────────┘                  │
                    │                          │                              │
                    │                    取最小时间戳                          │
                    │                          │                              │
                    │          ┌───────────────┴───────────────┐              │
                    │          ▼                               ▼              │
                    │  ┌───────────────┐               ┌───────────────┐      │
                    │  │ Exchange Event│               │  Local Event  │      │
                    │  └───────┬───────┘               └───────┬───────┘      │
                    │          │                               │              │
                    │          ▼                               ▼              │
                    │  ┌───────────────┐               ┌───────────────┐      │
                    │  │   OrderBook   │               │StrategyExecutor│     │
                    │  │ (同步调用)    │               │  (同步调用)   │      │
                    │  └───────┬───────┘               └───────┬───────┘      │
                    │          │                               │              │
                    │          │ OrderUpdate                   │ Order        │
                    │          │ (+delay)                      │ (+delay)     │
                    │          ▼                               ▼              │
                    │  ┌───────────────┐               ┌───────────────┐      │
                    │  │ Local Queue   │               │Exchange Queue │      │
                    │  │ (插入)        │               │  (插入)       │      │
                    │  └───────────────┘               └───────────────┘      │
                    └─────────────────────────────────────────────────────────┘

延迟模型示例 (延迟=5ms):
─────────────────────────────────────────────────────────────────────────────
时间轴:  100   101   102   103   104   105   106   107   108   109   110
─────────────────────────────────────────────────────────────────────────────
交易所:  BBO                                         OrderReq      BBO
         │                                           ↑             │
         │ +5ms                            策略下单  │ +5ms        │ +5ms
         ↓                                 (T=103)   │             ↓
本地:              策略看到BBO────────▶下单─────────┘            策略看到BBO
                   (T=105)                                        (T=115)
─────────────────────────────────────────────────────────────────────────────
```

## 8. API 设计

### 8.1 启动回测

```rust
use hft_engine_rs::engine::backtest::{
    BacktestManager, BacktestConfig, LatencyConfig,
    CsvDataSource, MemoryDataSource,
};

// 方式1：使用 Builder 模式
let result = BacktestManager::new(BacktestConfig {
    start_time: parse_timestamp("2024-01-01 00:00:00"),
    end_time: parse_timestamp("2024-01-31 23:59:59"),
    latency: LatencyConfig {
        exchange_to_local_ms: 5,
        local_to_exchange_ms: 5,
    },
    initial_capital: hashmap! {
        Exchange::Binance => 10000.0,
        Exchange::Okx => 10000.0,
    },
    fee_config: FeeConfig::default(),
    clock_interval_ms: 1000,
})
.add_data_source(Box::new(CsvDataSource::new(
    "binance_btc_bbo",
    Exchange::Binance,
    Symbol::new("BTC", "USDT"),
    DataType::BBO,
    "data/binance_btc_bbo.csv",
)))
.add_data_source(Box::new(CsvDataSource::new(
    "binance_btc_funding",
    Exchange::Binance,
    Symbol::new("BTC", "USDT"),
    DataType::FundingRate,
    "data/binance_btc_funding.csv",
)))
.add_strategy(Box::new(FundingArbStrategy::new(strategy_config)))
.run()?;

// 输出结果
println!("=== Backtest Result ===");
println!("Period: {} - {}", result.start_time, result.end_time);
println!("Total PnL: {:.2}", result.metrics.total_pnl);
println!("Total Fee: {:.2}", result.metrics.total_fee);
println!("Win Rate: {:.2}%", result.metrics.win_rate * 100.0);
println!("Max Drawdown: {:.2}%", result.metrics.max_drawdown * 100.0);
println!("Sharpe Ratio: {:.2}", result.metrics.sharpe_ratio);
println!("Total Trades: {}", result.metrics.total_trades);
```

### 8.2 使用内存数据源（单元测试）

```rust
#[test]
fn test_simple_strategy() {
    let events = vec![
        IncomeEvent {
            exchange_ts: 1000,
            local_ts: 1000,
            data: ExchangeEventData::BBO(BBO {
                exchange: Exchange::Binance,
                symbol: Symbol::new("BTC", "USDT"),
                bid_price: 99.0,
                bid_qty: 10.0,
                ask_price: 100.0,
                ask_qty: 10.0,
                timestamp: 1000,
            }),
        },
        // ... more events
    ];

    let result = BacktestManager::new(BacktestConfig::default())
        .add_data_source(Box::new(MemoryDataSource::new("test", events)))
        .add_strategy(Box::new(TestStrategy::new()))
        .run()
        .unwrap();

    assert!(result.metrics.total_pnl > 0.0);
}
```

## 9. 待讨论问题

### Q1: 多数据源时序对齐

当有多个数据源（如 Binance BBO + OKX BBO + Funding Rate）时，如何保证时序正确？

**已解决**：
- 所有数据源在回测开始前**预加载**到 exchange_queue
- 事件队列自动按时间戳排序
- 无需 watermark 机制，因为数据已全部加载

### Q2: 资金费结算时机

资金费是在特定时间点（如8小时一次）结算，如何在回测中处理？

**建议**：
- FundingRate 事件包含 `next_settle_time` 字段
- 在本地队列中生成 `FundingSettle` 事件（按结算时间）
- 结算时计算费用：`position.size * funding_rate * mark_price`

### Q3: 延迟配置粒度

是否需要支持不同交易所、不同方向的差异化延迟？

**可选方案**：
```rust
// 方案A：统一延迟
pub struct LatencyConfig {
    pub exchange_to_local_ms: u64,
    pub local_to_exchange_ms: u64,
}

// 方案B：按交易所差异化
pub struct LatencyConfig {
    pub default_exchange_to_local_ms: u64,
    pub default_local_to_exchange_ms: u64,
    pub overrides: HashMap<Exchange, ExchangeLatency>,
}
```

建议初始版本使用方案A，后续按需扩展。

### Q4: 同一时间戳事件的处理顺序

同一毫秒内可能有多个事件，如何确定顺序？

**已解决**：
- 使用 `sequence` 字段保证插入顺序
- 同时间戳事件按插入顺序处理（FIFO）
- 这与实盘行为一致（先到先处理）

### Q5: 撮合模型的精确度

简化L1模型 vs 完整订单簿模型？

**建议**：
- 初始版本使用简化L1模型（已设计）
- 假设：订单数量不会影响市场价格（小单假设）
- 后续可扩展：加入滑点模型、部分成交模拟

## 10. 实现计划

### Phase 1: 核心框架
- [ ] 事件类型定义 (ExchangeEvent, LocalEvent, TimedEvent)
- [ ] BacktestProcessor 结构和主循环
- [ ] 双队列管理和时序控制

### Phase 2: 订单簿模拟
- [ ] OrderBook 结构
- [ ] 市价单撮合
- [ ] 限价单挂单和触发撮合

### Phase 3: 策略执行
- [ ] StrategyExecutor 结构
- [ ] StateManager 集成
- [ ] 订单转换逻辑

### Phase 4: 数据源
- [ ] DataSource trait 定义
- [ ] MemoryDataSource 实现（测试用）
- [ ] CsvDataSource 实现

### Phase 5: 集成与测试
- [ ] BacktestManager 入口
- [ ] 单策略端到端测试
- [ ] 多交易所场景测试

### Phase 6: 统计与优化
- [ ] ResultCollector 实现
- [ ] 统计指标计算
- [ ] 性能分析和优化

## 11. 文件结构规划

```
src/engine/
├── live/                    # 现有实盘模块
│   ├── mod.rs
│   ├── engine.rs
│   ├── processor.rs
│   ├── executor.rs
│   ├── signal_processor.rs
│   └── clock.rs
└── backtest/               # 新增回测模块
    ├── mod.rs              # 模块导出
    ├── event.rs            # ExchangeEvent, LocalEvent, TimedEvent
    ├── processor.rs        # BacktestProcessor (核心引擎)
    ├── order_book.rs       # OrderBook (撮合模拟)
    ├── executor.rs         # StrategyExecutor
    ├── manager.rs          # BacktestManager (入口)
    ├── result.rs           # BacktestResult, ResultCollector
    ├── config.rs           # BacktestConfig, LatencyConfig
    └── data_source/        # 数据源
        ├── mod.rs          # DataSource trait
        ├── memory.rs       # MemoryDataSource
        └── csv.rs          # CsvDataSource
```

## 12. 设计决策总结

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 事件队列 | 双队列（交易所+本地） | 精确模拟网络延迟 |
| 处理模式 | 同步单线程 | 保证确定性，避免并发乱序 |
| 组件形式 | struct（非Actor） | 同步调用，精确控制时序 |
| 数据加载 | 预加载 | 简化实现，避免流式复杂性 |
| 撮合模型 | 简化L1 | 初始版本，后续可扩展 |
| 延迟模型 | 统一配置 | 初始版本，后续可按交易所差异化 |

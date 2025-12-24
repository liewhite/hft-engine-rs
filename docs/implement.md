# Rust 重写开发指南

本指南基于 Scala/ZIO 实现的 funding-arb 项目，为 Rust 重写提供完整的技术参考。

## 目录

1. [架构概述](#架构概述)
2. [核心 Domain 模型](#核心-domain-模型)
3. [交易所抽象层](#交易所抽象层)
4. [Hub 消息系统](#hub-消息系统)
5. [Binance 实现细节](#binance-实现细节)
6. [OKX 实现细节](#okx-实现细节)
7. [组件生命周期管理](#组件生命周期管理)
8. [Rust 实现建议](#rust-实现建议)

---

## 架构概述

### 数据流架构

```
┌─────────────────────────────────────────────────────┐
│           Exchange WebSockets (tokio-tungstenite)   │
│           (Binance, OKX)                            │
└──────────────────────┬──────────────────────────────┘
                       │ async stream
                       ▼
┌─────────────────────────────────────────────────────┐
│        PublicHub / PrivateHub (broadcast channel)   │
│  - funding_rates: broadcast::Sender<FundingRate>    │
│  - bbos: broadcast::Sender<BBO>                     │
│  - positions: broadcast::Sender<Position>           │
│  - balances: broadcast::Sender<Balance>             │
│  - order_updates: broadcast::Sender<OrderUpdate>    │
└──────────────────────┬──────────────────────────────┘
                       │ subscribe
                       ▼
┌─────────────────────────────────────────────────────┐
│          EventDispatcher (路由转换层)                │
│    将 Hub 事件转换为 SymbolEvent 并分发              │
└──────────────────────┬──────────────────────────────┘
                       │ publish
                       ▼
┌─────────────────────────────────────────────────────┐
│       SymbolEventBus (per-symbol bounded channel)   │
│   - 每个 Symbol 一个 bounded channel                │
│   - 满时丢弃旧消息 (sliding window)                  │
└──────────────────────┬──────────────────────────────┘
                       │ subscribe
                       ▼
┌─────────────────────────────────────────────────────┐
│       SymbolProcessor (per-symbol tokio task)       │
│   1. 更新 SymbolState                               │
│   2. 评估策略                                        │
│   3. 风险检查                                        │
│   4. 执行交易                                        │
└─────────────────────────────────────────────────────┘
```

### 核心设计原则

1. **Hub-based Broadcast**: 使用 `tokio::sync::broadcast` 实现多订阅者发布订阅
2. **Per-symbol 隔离**: 每个交易对由独立的 task 处理
3. **Sliding Queue**: 有界队列，满时丢弃最旧事件
4. **Trait-driven 抽象**: 交易所功能通过 trait 统一
5. **纯函数策略**: 策略评估无副作用

---

## 核心 Domain 模型

### 基础类型 (Newtype Pattern)

```rust
use rust_decimal::Decimal;
use std::time::Instant;

// 使用 newtype pattern 保证类型安全
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrderId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Quantity(pub Decimal);

impl Quantity {
    pub const ZERO: Self = Self(Decimal::ZERO);

    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    pub fn abs(&self) -> Self {
        Self(self.0.abs())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Price(pub Decimal);

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Rate(pub Decimal);

impl Rate {
    pub const ZERO: Self = Self(Decimal::ZERO);

    pub fn abs(&self) -> Self {
        Self(self.0.abs())
    }
}
```

### 交易所枚举

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exchange {
    Binance,
    OKX,
    // 未来扩展: Bybit, Bitget, Gate...
}
```

### 交易方向

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }
}
```

### 订单类型

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    GTC,      // Good Till Cancel
    IOC,      // Immediate or Cancel
    FOK,      // Fill or Kill
    PostOnly, // Maker Only
}

#[derive(Debug, Clone)]
pub enum OrderType {
    Market,
    Limit { price: Price, tif: TimeInForce },
}

#[derive(Debug, Clone)]
pub enum OrderStatus {
    Pending,
    PartiallyFilled { filled: Quantity },
    Filled,
    Cancelled,
    Rejected { reason: String },
}
```

### 核心数据结构

```rust
/// 统一交易对符号
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Symbol {
    pub base: String,   // e.g., "BTC"
    pub quote: String,  // e.g., "USDT"
}

impl Symbol {
    pub fn canonical(&self) -> String {
        format!("{}_{}", self.base, self.quote)
    }

    pub fn from_canonical(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('_').collect();
        if parts.len() == 2 {
            Some(Symbol {
                base: parts[0].to_string(),
                quote: parts[1].to_string(),
            })
        } else {
            None
        }
    }
}

/// 资金费率
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub rate: Rate,
    pub next_settle_time: Instant,
}

impl FundingRate {
    /// 年化费率 (假设每天3次结算)
    pub fn annualized_rate(&self) -> Rate {
        Rate(self.rate.0 * Decimal::from(3 * 365))
    }
}

/// 仓位信息
#[derive(Debug, Clone)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub size: Quantity,
    pub entry_price: Price,
    pub leverage: u32,
    pub unrealized_pnl: Decimal,
    pub mark_price: Price,
}

impl Position {
    pub fn notional_value(&self) -> Decimal {
        self.mark_price.0 * self.size.0
    }

    pub fn is_empty(&self) -> bool {
        self.size.is_zero()
    }

    pub fn empty(exchange: Exchange, symbol: Symbol) -> Self {
        Self {
            exchange,
            symbol,
            side: Side::Long,
            size: Quantity::ZERO,
            entry_price: Price(Decimal::ZERO),
            leverage: 1,
            unrealized_pnl: Decimal::ZERO,
            mark_price: Price(Decimal::ZERO),
        }
    }
}

/// 账户余额
#[derive(Debug, Clone)]
pub struct Balance {
    pub exchange: Exchange,
    pub asset: String,
    pub available: Decimal,
    pub frozen: Decimal,
}

impl Balance {
    pub fn total(&self) -> Decimal {
        self.available + self.frozen
    }
}

/// Best Bid Offer (L1 行情)
#[derive(Debug, Clone)]
pub struct BBO {
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub bid_price: Price,
    pub bid_qty: Quantity,
    pub ask_price: Price,
    pub ask_qty: Quantity,
    pub timestamp: Instant,
}

impl BBO {
    pub fn spread(&self) -> Price {
        Price(self.ask_price.0 - self.bid_price.0)
    }

    pub fn mid_price(&self) -> Price {
        Price((self.bid_price.0 + self.ask_price.0) / Decimal::from(2))
    }
}

/// 订单
#[derive(Debug, Clone)]
pub struct Order {
    pub id: OrderId,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub side: Side,
    pub order_type: OrderType,
    pub quantity: Quantity,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
}

/// 订单更新事件
#[derive(Debug, Clone)]
pub struct OrderUpdate {
    pub order_id: OrderId,
    pub exchange: Exchange,
    pub symbol: Symbol,
    pub status: OrderStatus,
    pub filled_quantity: Quantity,
    pub avg_price: Option<Price>,
    pub timestamp: Instant,
}
```

---

## 交易所抽象层

### 核心 Trait 定义

```rust
use async_trait::async_trait;
use tokio::sync::broadcast;

/// Public 市场数据 Hub
pub struct PublicHubs {
    pub funding_rates: broadcast::Sender<FundingRate>,
    pub bbos: broadcast::Sender<BBO>,
}

impl PublicHubs {
    pub fn new(capacity: usize) -> Self {
        Self {
            funding_rates: broadcast::channel(capacity).0,
            bbos: broadcast::channel(capacity).0,
        }
    }
}

/// Private 账户数据 Hub
pub struct PrivateHubs {
    pub positions: broadcast::Sender<Position>,
    pub balances: broadcast::Sender<Balance>,
    pub order_updates: broadcast::Sender<OrderUpdate>,
}

impl PrivateHubs {
    pub fn new(capacity: usize) -> Self {
        Self {
            positions: broadcast::channel(capacity).0,
            balances: broadcast::channel(capacity).0,
            order_updates: broadcast::channel(capacity).0,
        }
    }
}

/// 交易所 WebSocket 连接 trait
#[async_trait]
pub trait ExchangeWebSocket: Send + Sync {
    fn exchange(&self) -> Exchange;

    /// 连接公共 WebSocket 并订阅市场数据
    /// 返回 PublicHubs 和一个 JoinHandle (用于生命周期管理)
    async fn connect_public(
        &self,
        symbols: &[Symbol],
        cancel_token: CancellationToken,
    ) -> Result<PublicHubs, ExchangeError>;

    /// 连接私有 WebSocket 并订阅账户数据
    async fn connect_private(
        &self,
        cancel_token: CancellationToken,
    ) -> Result<PrivateHubs, ExchangeError>;
}

/// 交易所执行器 trait
#[async_trait]
pub trait ExchangeExecutor: Send + Sync {
    fn exchange(&self) -> Exchange;

    /// 下单，返回交易所分配的订单ID
    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError>;

    /// 设置杠杆
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError>;
}

/// 组合接口
pub trait ExchangeAdapter: ExchangeWebSocket + ExchangeExecutor {}
```

### 错误类型

```rust
use thiserror::Error;
use std::time::Duration;

#[derive(Debug, Error)]
pub enum ExchangeError {
    #[error("Connection failed to {0:?}: {1}")]
    ConnectionFailed(Exchange, String),

    #[error("Authentication failed for {0:?}")]
    AuthenticationFailed(Exchange),

    #[error("Rate limited on {0:?}, retry after {1:?}")]
    RateLimited(Exchange, Duration),

    #[error("Order rejected on {0:?}: {1}")]
    OrderRejected(Exchange, String),

    #[error("Insufficient balance on {0:?}: need {1}, have {2}")]
    InsufficientBalance(Exchange, Decimal, Decimal),

    #[error("Symbol not found on {0:?}: {1}")]
    SymbolNotFound(Exchange, String),

    #[error("API error from {0:?}: code={1}, msg={2}")]
    ApiError(Exchange, i32, String),
}
```

---

## Hub 消息系统

### SymbolEvent 统一事件类型

```rust
use std::time::Instant;

/// 统一的 Symbol 事件类型
#[derive(Debug, Clone)]
pub enum SymbolEvent {
    FundingRateUpdate {
        symbol: Symbol,
        exchange: Exchange,
        rate: FundingRate,
        timestamp: Instant,
    },
    BBOUpdate {
        symbol: Symbol,
        exchange: Exchange,
        bbo: BBO,
        timestamp: Instant,
    },
    PositionUpdate {
        symbol: Symbol,
        exchange: Exchange,
        position: Position,
        timestamp: Instant,
    },
    OrderStatusUpdate {
        symbol: Symbol,
        exchange: Exchange,
        update: OrderUpdate,
        timestamp: Instant,
    },
    BalanceUpdate {
        symbol: Symbol,
        exchange: Exchange,
        balance: Balance,
        timestamp: Instant,
    },
}

impl SymbolEvent {
    pub fn symbol(&self) -> &Symbol {
        match self {
            Self::FundingRateUpdate { symbol, .. } => symbol,
            Self::BBOUpdate { symbol, .. } => symbol,
            Self::PositionUpdate { symbol, .. } => symbol,
            Self::OrderStatusUpdate { symbol, .. } => symbol,
            Self::BalanceUpdate { symbol, .. } => symbol,
        }
    }

    pub fn exchange(&self) -> Exchange {
        match self {
            Self::FundingRateUpdate { exchange, .. } => *exchange,
            Self::BBOUpdate { exchange, .. } => *exchange,
            Self::PositionUpdate { exchange, .. } => *exchange,
            Self::OrderStatusUpdate { exchange, .. } => *exchange,
            Self::BalanceUpdate { exchange, .. } => *exchange,
        }
    }
}
```

### SymbolState 聚合状态

```rust
use std::collections::HashMap;

/// 单个交易对在所有交易所的聚合状态
#[derive(Debug, Clone)]
pub struct SymbolState {
    pub symbol: Symbol,
    pub funding_rates: HashMap<Exchange, FundingRate>,
    pub bbos: HashMap<Exchange, BBO>,
    pub positions: HashMap<Exchange, Position>,
    pub pending_orders: HashMap<(Exchange, OrderId), Order>,
}

impl SymbolState {
    pub fn empty(symbol: Symbol) -> Self {
        Self {
            symbol,
            funding_rates: HashMap::new(),
            bbos: HashMap::new(),
            positions: HashMap::new(),
            pending_orders: HashMap::new(),
        }
    }

    /// 获取费率最高的交易所 (适合做空)
    pub fn best_short_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        self.funding_rates
            .iter()
            .max_by(|a, b| a.1.rate.0.cmp(&b.1.rate.0))
            .map(|(e, r)| (*e, r))
    }

    /// 获取费率最低的交易所 (适合做多)
    pub fn best_long_exchange(&self) -> Option<(Exchange, &FundingRate)> {
        self.funding_rates
            .iter()
            .min_by(|a, b| a.1.rate.0.cmp(&b.1.rate.0))
            .map(|(e, r)| (*e, r))
    }

    /// 计算费率差
    pub fn funding_spread(&self) -> Option<Rate> {
        let (_, high) = self.best_short_exchange()?;
        let (_, low) = self.best_long_exchange()?;
        Some(Rate(high.rate.0 - low.rate.0))
    }

    /// 是否有持仓
    pub fn has_positions(&self) -> bool {
        self.positions.values().any(|p| !p.is_empty())
    }

    /// 更新状态
    pub fn update(&mut self, event: SymbolEvent) {
        match event {
            SymbolEvent::FundingRateUpdate { exchange, rate, .. } => {
                self.funding_rates.insert(exchange, rate);
            }
            SymbolEvent::BBOUpdate { exchange, bbo, .. } => {
                self.bbos.insert(exchange, bbo);
            }
            SymbolEvent::PositionUpdate { exchange, position, .. } => {
                self.positions.insert(exchange, position);
            }
            SymbolEvent::OrderStatusUpdate { exchange, update, .. } => {
                match update.status {
                    OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected { .. } => {
                        self.pending_orders.remove(&(exchange, update.order_id));
                    }
                    _ => {}
                }
            }
            SymbolEvent::BalanceUpdate { .. } => {
                // Balance 在全局追踪，不在 per-symbol 状态中
            }
        }
    }
}
```

### SymbolEventBus 实现

```rust
use tokio::sync::mpsc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-symbol 事件队列统计
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub capacity: usize,
    pub dropped_count: u64,
}

/// Symbol 事件总线 - 管理 per-symbol 的有界队列
pub struct SymbolEventBus {
    queues: HashMap<Symbol, (mpsc::Sender<SymbolEvent>, AtomicU64)>,
    capacity: usize,
}

impl SymbolEventBus {
    pub fn new(symbols: &[Symbol], capacity: usize) -> (Self, HashMap<Symbol, mpsc::Receiver<SymbolEvent>>) {
        let mut queues = HashMap::new();
        let mut receivers = HashMap::new();

        for symbol in symbols {
            let (tx, rx) = mpsc::channel(capacity);
            queues.insert(symbol.clone(), (tx, AtomicU64::new(0)));
            receivers.insert(symbol.clone(), rx);
        }

        (Self { queues, capacity }, receivers)
    }

    /// 发布事件到对应 symbol 的队列
    pub async fn publish(&self, event: SymbolEvent) {
        if let Some((sender, dropped_counter)) = self.queues.get(event.symbol()) {
            // 使用 try_send 实现 sliding window 语义
            if sender.try_send(event).is_err() {
                dropped_counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// 获取队列统计
    pub fn stats(&self) -> HashMap<Symbol, QueueStats> {
        self.queues
            .iter()
            .map(|(symbol, (_, counter))| {
                (symbol.clone(), QueueStats {
                    capacity: self.capacity,
                    dropped_count: counter.load(Ordering::Relaxed),
                })
            })
            .collect()
    }
}
```

### EventDispatcher 实现

```rust
use tokio_util::sync::CancellationToken;

pub struct EventDispatcher;

impl EventDispatcher {
    /// 从 PublicHubs 分发事件到 SymbolEventBus
    pub fn dispatch_public(
        exchange: Exchange,
        hubs: &PublicHubs,
        event_bus: Arc<SymbolEventBus>,
        cancel_token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let mut funding_rx = hubs.funding_rates.subscribe();
        let event_bus = event_bus.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    result = funding_rx.recv() => {
                        match result {
                            Ok(rate) => {
                                let event = SymbolEvent::FundingRateUpdate {
                                    symbol: rate.symbol.clone(),
                                    exchange,
                                    rate,
                                    timestamp: Instant::now(),
                                };
                                event_bus.publish(event).await;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        }
                    }
                }
            }
        })
    }

    /// 从 PrivateHubs 分发事件到 SymbolEventBus
    pub fn dispatch_private(
        exchange: Exchange,
        hubs: &PrivateHubs,
        event_bus: Arc<SymbolEventBus>,
        cancel_token: CancellationToken,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();

        // Position updates
        {
            let mut rx = hubs.positions.subscribe();
            let bus = event_bus.clone();
            let token = cancel_token.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            if let Ok(pos) = result {
                                let event = SymbolEvent::PositionUpdate {
                                    symbol: pos.symbol.clone(),
                                    exchange,
                                    position: pos,
                                    timestamp: Instant::now(),
                                };
                                bus.publish(event).await;
                            }
                        }
                    }
                }
            }));
        }

        // Order updates
        {
            let mut rx = hubs.order_updates.subscribe();
            let bus = event_bus.clone();
            let token = cancel_token.clone();

            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = rx.recv() => {
                            if let Ok(update) = result {
                                let event = SymbolEvent::OrderStatusUpdate {
                                    symbol: update.symbol.clone(),
                                    exchange,
                                    update,
                                    timestamp: Instant::now(),
                                };
                                bus.publish(event).await;
                            }
                        }
                    }
                }
            }));
        }

        handles
    }
}
```

---

## Binance 实现细节

### URL 配置

```rust
pub mod binance {
    pub const REST_BASE_URL: &str = "https://fapi.binance.com";
    pub const REST_TESTNET_URL: &str = "https://testnet.binancefuture.com";

    pub const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";
    pub const WS_TESTNET_PUBLIC_URL: &str = "wss://fstream.binancefuture.com/ws";
}
```

### Symbol 格式转换

```rust
impl Symbol {
    /// Symbol("BTC", "USDT") -> "BTCUSDT"
    pub fn to_binance(&self) -> String {
        format!("{}{}", self.base, self.quote)
    }

    /// "BTCUSDT" -> Symbol("BTC", "USDT")
    pub fn from_binance(s: &str) -> Option<Self> {
        const KNOWN_QUOTES: [&str; 4] = ["USDT", "BUSD", "USDC", "USD"];

        for quote in KNOWN_QUOTES {
            if s.ends_with(quote) {
                let base = &s[..s.len() - quote.len()];
                return Some(Symbol {
                    base: base.to_string(),
                    quote: quote.to_string(),
                });
            }
        }
        None
    }
}
```

### REST API 签名

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn sign_binance(query_string: &str, secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(query_string.as_bytes());
    let result = mac.finalize();
    hex::encode(result.into_bytes())
}

pub fn build_signed_query(params: &[(&str, &str)], secret: &str) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let mut query_parts: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();
    query_parts.push(format!("timestamp={}", timestamp));

    let query_string = query_parts.join("&");
    let signature = sign_binance(&query_string, secret);

    format!("{}&signature={}", query_string, signature)
}
```

### REST API Endpoints

| 操作 | 方法 | Endpoint | 参数 |
|-----|------|----------|------|
| 下单 | POST | `/fapi/v1/order` | symbol, side, type, quantity, timeInForce, price?, reduceOnly |
| 设置杠杆 | POST | `/fapi/v1/leverage` | symbol, leverage |
| 创建 ListenKey | POST | `/fapi/v1/listenKey` | - |
| 续期 ListenKey | PUT | `/fapi/v1/listenKey` | - |
| 查询仓位 | GET | `/fapi/v3/positionRisk` | - |
| 查询余额 | GET | `/fapi/v3/balance` | - |

**请求头**: `X-MBX-APIKEY: <api_key>`

### 订单类型映射

```rust
pub fn order_type_to_binance(order_type: &OrderType) -> (&'static str, Option<String>, Option<&'static str>) {
    match order_type {
        OrderType::Market => ("MARKET", None, None),
        OrderType::Limit { price, tif } => {
            let tif_str = match tif {
                TimeInForce::GTC => "GTC",
                TimeInForce::IOC => "IOC",
                TimeInForce::FOK => "FOK",
                TimeInForce::PostOnly => "GTX", // Binance 用 GTX 表示 PostOnly
            };
            ("LIMIT", Some(price.0.to_string()), Some(tif_str))
        }
    }
}

pub fn side_to_binance(side: Side) -> &'static str {
    match side {
        Side::Long => "BUY",
        Side::Short => "SELL",
    }
}
```

### WebSocket 订阅消息格式

**订阅请求**:
```json
{
  "method": "SUBSCRIBE",
  "params": ["btcusdt@markPrice", "btcusdt@bookTicker"],
  "id": 1
}
```

### WebSocket 数据格式

**Mark Price (资金费率)**:
```rust
#[derive(Debug, Deserialize)]
pub struct MarkPriceUpdate {
    pub e: String,      // "markPriceUpdate"
    pub s: String,      // "BTCUSDT"
    pub p: String,      // mark price
    pub i: String,      // index price
    pub r: String,      // funding rate (当前费率)
    #[serde(rename = "T")]
    pub t: i64,         // next funding time (毫秒时间戳)
}
```

**Book Ticker (BBO)**:
```rust
#[derive(Debug, Deserialize)]
pub struct BookTicker {
    pub e: String,      // "bookTicker"
    pub s: String,      // "BTCUSDT"
    pub b: String,      // best bid price
    #[serde(rename = "B")]
    pub bid_qty: String, // best bid qty
    pub a: String,      // best ask price
    #[serde(rename = "A")]
    pub ask_qty: String, // best ask qty
    #[serde(rename = "T")]
    pub t: i64,         // timestamp
}
```

**账户更新 (ACCOUNT_UPDATE)**:
```rust
#[derive(Debug, Deserialize)]
pub struct AccountUpdate {
    pub e: String,      // "ACCOUNT_UPDATE"
    pub a: AccountData,
}

#[derive(Debug, Deserialize)]
pub struct AccountData {
    #[serde(rename = "B")]
    pub balances: Vec<AccountBalance>,
    #[serde(rename = "P")]
    pub positions: Vec<AccountPosition>,
}

#[derive(Debug, Deserialize)]
pub struct AccountBalance {
    pub a: String,      // asset
    pub wb: String,     // wallet balance
    pub cw: String,     // cross wallet balance
    pub bc: String,     // balance change
}

#[derive(Debug, Deserialize)]
pub struct AccountPosition {
    pub s: String,      // symbol
    pub pa: String,     // position amount
    pub ep: String,     // entry price
    pub up: String,     // unrealized pnl
    pub mt: String,     // margin type
    pub ps: String,     // position side
}
```

**订单更新 (ORDER_TRADE_UPDATE)**:
```rust
#[derive(Debug, Deserialize)]
pub struct OrderTradeUpdate {
    pub e: String,      // "ORDER_TRADE_UPDATE"
    pub o: OrderData,
}

#[derive(Debug, Deserialize)]
pub struct OrderData {
    pub s: String,      // symbol
    pub i: i64,         // order id
    #[serde(rename = "X")]
    pub status: String, // NEW/PARTIALLY_FILLED/FILLED/CANCELED/REJECTED
    pub q: String,      // original qty
    pub z: String,      // filled qty
    pub ap: String,     // avg price
    pub rp: String,     // realized profit
}
```

### User Data Stream 管理

```rust
pub struct BinanceUserDataStream {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    listen_key: Option<String>,
}

impl BinanceUserDataStream {
    /// 创建 ListenKey
    pub async fn create_listen_key(&mut self) -> Result<String, ExchangeError> {
        let resp = self.client
            .post(format!("{}/fapi/v1/listenKey", self.base_url))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        #[derive(Deserialize)]
        struct Response { listen_key: String }

        let data: Response = resp.json().await?;
        self.listen_key = Some(data.listen_key.clone());
        Ok(data.listen_key)
    }

    /// 续期 ListenKey (每30分钟调用一次)
    pub async fn keep_alive(&self) -> Result<(), ExchangeError> {
        self.client
            .put(format!("{}/fapi/v1/listenKey", self.base_url))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;
        Ok(())
    }

    /// 获取 Private WebSocket URL
    pub fn ws_url(&self) -> Option<String> {
        self.listen_key.as_ref().map(|key| {
            format!("wss://fstream.binance.com/ws/{}", key)
        })
    }
}
```

### 错误码映射

```rust
pub fn map_binance_error(code: i32, msg: &str) -> ExchangeError {
    match code {
        -1003 => ExchangeError::RateLimited(Exchange::Binance, Duration::from_secs(60)),
        -2010 | -2019 => ExchangeError::InsufficientBalance(
            Exchange::Binance,
            Decimal::ZERO,
            Decimal::ZERO
        ),
        -4028 => ExchangeError::ApiError(
            Exchange::Binance,
            code,
            format!("Leverage exceeded: {}", msg)
        ),
        _ => ExchangeError::ApiError(Exchange::Binance, code, msg.to_string()),
    }
}
```

---

## OKX 实现细节

### URL 配置

```rust
pub mod okx {
    pub const REST_BASE_URL: &str = "https://www.okx.com";
    pub const REST_DEMO_URL: &str = "https://www.okx.com"; // Demo 用相同 URL，通过 header 区分

    pub const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
    pub const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";
    pub const WS_DEMO_PUBLIC_URL: &str = "wss://wspap.okx.com:8443/ws/v5/public?brokerId=9999";
    pub const WS_DEMO_PRIVATE_URL: &str = "wss://wspap.okx.com:8443/ws/v5/private?brokerId=9999";
}
```

### Symbol 格式转换

```rust
impl Symbol {
    /// Symbol("BTC", "USDT") -> "BTC-USDT-SWAP"
    pub fn to_okx(&self) -> String {
        format!("{}-{}-SWAP", self.base, self.quote)
    }

    /// "BTC-USDT-SWAP" -> Symbol("BTC", "USDT")
    pub fn from_okx(inst_id: &str) -> Option<Self> {
        let parts: Vec<&str> = inst_id.split('-').collect();
        if parts.len() == 3 && parts[2] == "SWAP" {
            Some(Symbol {
                base: parts[0].to_string(),
                quote: parts[1].to_string(),
            })
        } else {
            None
        }
    }
}
```

### REST API 签名

```rust
use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;

/// ISO 8601 格式时间戳
pub fn iso_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Unix 时间戳 (秒，用于 WebSocket 登录)
pub fn unix_timestamp() -> String {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs())
    .to_string()
}

/// REST API 签名: Base64(HMAC_SHA256(timestamp + method + path + body, secret))
pub fn sign_okx_rest(timestamp: &str, method: &str, path: &str, body: &str, secret: &str) -> String {
    let message = format!("{}{}{}{}", timestamp, method, path, body);

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    let result = mac.finalize();

    general_purpose::STANDARD.encode(result.into_bytes())
}

/// WebSocket 登录签名: Base64(HMAC_SHA256(timestamp + 'GET' + '/users/self/verify', secret))
pub fn sign_okx_ws_login(timestamp: &str, secret: &str) -> String {
    let message = format!("{}GET/users/self/verify", timestamp);

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(message.as_bytes());
    let result = mac.finalize();

    general_purpose::STANDARD.encode(result.into_bytes())
}
```

### REST API 请求头

```rust
fn build_okx_headers(
    api_key: &str,
    sign: &str,
    timestamp: &str,
    passphrase: &str,
) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("OK-ACCESS-KEY", api_key.parse().unwrap());
    headers.insert("OK-ACCESS-SIGN", sign.parse().unwrap());
    headers.insert("OK-ACCESS-TIMESTAMP", timestamp.parse().unwrap());
    headers.insert("OK-ACCESS-PASSPHRASE", passphrase.parse().unwrap());
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers
}
```

### REST API Endpoints

| 操作 | 方法 | Endpoint | 参数 |
|-----|------|----------|------|
| 下单 | POST | `/api/v5/trade/order` | instId, tdMode, side, ordType, sz, px?, reduceOnly, clOrdId? |
| 设置杠杆 | POST | `/api/v5/account/set-leverage` | instId, lever, mgnMode |

### 订单类型映射

```rust
/// OKX 将 TimeInForce 合并到 ordType 中
pub fn order_type_to_okx(order_type: &OrderType) -> (&'static str, Option<String>) {
    match order_type {
        OrderType::Market => ("market", None),
        OrderType::Limit { price, tif } => {
            let ord_type = match tif {
                TimeInForce::GTC => "limit",      // 默认 GTC
                TimeInForce::IOC => "ioc",
                TimeInForce::FOK => "fok",
                TimeInForce::PostOnly => "post_only",
            };
            (ord_type, Some(price.0.to_string()))
        }
    }
}

pub fn side_to_okx(side: Side) -> &'static str {
    match side {
        Side::Long => "buy",
        Side::Short => "sell",
    }
}
```

### REST API 响应格式

```rust
#[derive(Debug, Deserialize)]
pub struct OkxResponse<T> {
    pub code: String,   // "0" = 成功
    pub msg: String,
    pub data: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderData {
    pub ord_id: String,
    pub cl_ord_id: Option<String>,
    pub s_code: String,  // "0" = 成功
    pub s_msg: String,
}
```

### WebSocket 订阅消息格式

**订阅请求**:
```json
{
  "op": "subscribe",
  "args": [
    {"channel": "funding-rate", "instId": "BTC-USDT-SWAP"},
    {"channel": "bbo-tbt", "instId": "BTC-USDT-SWAP"}
  ]
}
```

**登录请求** (Private WebSocket):
```json
{
  "op": "login",
  "args": [{
    "apiKey": "xxx",
    "passphrase": "xxx",
    "timestamp": "1672531200",
    "sign": "xxx"
  }]
}
```

**登录成功响应**:
```json
{
  "event": "login",
  "code": "0",
  "msg": ""
}
```

### WebSocket 数据格式

**通用推送格式**:
```rust
#[derive(Debug, Deserialize)]
pub struct WsPush<T> {
    pub arg: WsArg,
    pub data: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsArg {
    pub channel: String,
    pub inst_id: Option<String>,
    pub inst_type: Option<String>,
}
```

**Funding Rate**:
```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingRateData {
    pub inst_id: String,           // "BTC-USDT-SWAP"
    pub inst_type: String,         // "SWAP"
    pub funding_rate: String,      // 当前费率
    pub next_funding_rate: Option<String>,  // 预测下期费率
    pub funding_time: String,      // 当前费率生效时间
    pub next_funding_time: String, // 下次结算时间 (毫秒时间戳)
}
```

**BBO (bbo-tbt)**:
```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BboData {
    pub asks: Vec<Vec<String>>,  // [[price, qty, _, _], ...]
    pub bids: Vec<Vec<String>>,  // [[price, qty, _, _], ...]
    pub ts: String,              // 时间戳
    pub seq_id: i64,
}
```

**Position**:
```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionData {
    pub inst_id: String,     // "BTC-USDT-SWAP"
    pub inst_type: String,   // "SWAP"
    pub pos: String,         // 持仓数量 (正=多，负=空)
    pub pos_side: String,    // "long"/"short"/"net"
    pub avg_px: String,      // 开仓均价
    pub upl: String,         // 未实现盈亏
    pub lever: String,       // 杠杆
    pub mgn_mode: String,    // "cross"/"isolated"
    pub mark_px: Option<String>, // 标记价格
}
```

**Account**:
```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    pub u_time: String,
    pub details: Vec<AccountDetail>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDetail {
    pub ccy: String,         // 币种
    pub eq: String,          // 总权益
    pub avail_eq: String,    // 可用权益
    pub avail_bal: String,   // 可用余额
    pub frozen_bal: String,  // 冻结余额
}
```

**Order**:
```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPushData {
    pub inst_id: String,
    pub ord_id: String,
    pub cl_ord_id: Option<String>,
    pub state: String,       // "live"/"partially_filled"/"filled"/"canceled"
    pub sz: String,          // 委托数量
    pub fill_sz: String,     // 成交数量
    pub avg_px: String,      // 成交均价
    pub fee: String,         // 手续费
    pub fee_ccy: String,
}
```

### 订单状态映射

```rust
pub fn map_okx_order_state(state: &str, fill_sz: &str) -> OrderStatus {
    let filled = fill_sz.parse::<Decimal>().unwrap_or(Decimal::ZERO);

    match state {
        "live" => OrderStatus::Pending,
        "partially_filled" => OrderStatus::PartiallyFilled { filled: Quantity(filled) },
        "filled" => OrderStatus::Filled,
        "canceled" | "cancelled" => OrderStatus::Cancelled,
        other => OrderStatus::Rejected { reason: format!("Unknown state: {}", other) },
    }
}
```

### 错误码映射

```rust
pub fn map_okx_error(code: &str, msg: &str) -> ExchangeError {
    match code {
        "50013" => ExchangeError::RateLimited(Exchange::OKX, Duration::from_secs(60)),
        "51020" => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            format!("Position limit exceeded: {}", msg)
        ),
        "51121" => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            format!("Order quantity exceeded: {}", msg)
        ),
        _ => ExchangeError::ApiError(
            Exchange::OKX,
            code.parse().unwrap_or(-1),
            msg.to_string()
        ),
    }
}
```

### OKX vs Binance 差异总结

| 方面 | Binance | OKX |
|-----|---------|-----|
| Symbol 格式 | `BTCUSDT` | `BTC-USDT-SWAP` |
| TIF 参数 | `timeInForce` 独立参数 | 合并到 `ordType` |
| WebSocket 快照 | 需 REST 获取 | **订阅时自动推送** |
| Private WS 认证 | Listen Key | 直接 WebSocket 登录 |
| 时间戳格式 | Unix ms | ISO 8601 |
| 签名格式 | HMAC-SHA256 Hex | HMAC-SHA256 Base64 |
| 签名内容 | Query string | timestamp + method + path + body |

---

## 组件生命周期管理

### CancellationToken 模式

```rust
use tokio_util::sync::CancellationToken;

pub struct ComponentLifecycle {
    token: CancellationToken,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl ComponentLifecycle {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            handles: Vec::new(),
        }
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub fn register(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    pub async fn shutdown(&self) {
        self.token.cancel();
        for handle in &self.handles {
            handle.abort();
        }
    }
}
```

### Hub 关闭检测

```rust
// 下游通过 Hub 关闭检测上游断开
pub async fn consume_from_hub(
    mut rx: broadcast::Receiver<FundingRate>,
    cancel_token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                tracing::info!("Cancelled by token");
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(rate) => {
                        // 处理数据
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Hub 已关闭，上游断开
                        tracing::warn!("Hub closed, upstream disconnected");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Lagged {} messages", n);
                        continue;
                    }
                }
            }
        }
    }
}
```

### 依赖关系管理

```rust
pub struct Coordinator {
    // 生命周期管理
    lifecycle: ComponentLifecycle,

    // 组件
    binance_ws: Arc<dyn ExchangeWebSocket>,
    okx_ws: Arc<dyn ExchangeWebSocket>,
    event_bus: Arc<SymbolEventBus>,
    processors: Vec<tokio::task::JoinHandle<()>>,
}

impl Coordinator {
    pub async fn start(&mut self, symbols: &[Symbol]) -> Result<(), ExchangeError> {
        let token = self.lifecycle.token();

        // 1. 连接交易所 WebSocket
        let binance_hubs = self.binance_ws.connect_public(symbols, token.clone()).await?;
        let okx_hubs = self.okx_ws.connect_public(symbols, token.clone()).await?;

        // 2. 启动事件分发器
        let dispatcher_handle = EventDispatcher::dispatch_public(
            Exchange::Binance,
            &binance_hubs,
            self.event_bus.clone(),
            token.clone(),
        );
        self.lifecycle.register(dispatcher_handle);

        // 3. 启动 per-symbol 处理器
        // ...

        Ok(())
    }

    pub async fn stop(&self) {
        self.lifecycle.shutdown().await;
    }
}
```

---

## Rust 实现建议

### 推荐依赖

```toml
[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
tokio-tungstenite = { version = "0.21", features = ["native-tls"] }

# HTTP client
reqwest = { version = "0.11", features = ["json", "native-tls"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Decimal arithmetic
rust_decimal = { version = "1", features = ["serde"] }

# Crypto
hmac = "0.12"
sha2 = "0.10"
hex = "0.4"
base64 = "0.21"

# Time
chrono = { version = "0.4", features = ["serde"] }

# Error handling
thiserror = "1"
anyhow = "1"

# Async trait
async-trait = "0.1"

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"
```

### 项目结构建议

```
src/
├── main.rs
├── lib.rs
├── domain/
│   ├── mod.rs
│   ├── types.rs          # Newtype 定义
│   ├── model.rs          # 核心数据结构
│   └── error.rs          # 错误类型
├── exchange/
│   ├── mod.rs
│   ├── api.rs            # Trait 定义
│   ├── binance/
│   │   ├── mod.rs
│   │   ├── codec.rs      # 消息编解码
│   │   ├── rest.rs       # REST API
│   │   └── ws.rs         # WebSocket
│   └── okx/
│       ├── mod.rs
│       ├── codec.rs
│       ├── rest.rs
│       └── ws.rs
├── messaging/
│   ├── mod.rs
│   ├── event.rs          # SymbolEvent
│   ├── event_bus.rs      # SymbolEventBus
│   └── dispatcher.rs     # EventDispatcher
├── engine/
│   ├── mod.rs
│   ├── coordinator.rs
│   └── processor.rs      # SymbolProcessor
├── strategy/
│   ├── mod.rs
│   ├── api.rs
│   └── funding_arb.rs
├── risk/
│   ├── mod.rs
│   └── rules.rs
└── config/
    └── mod.rs
```

### WebSocket 实现模式

```rust
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{StreamExt, SinkExt};

pub async fn connect_and_process<F, Fut>(
    url: &str,
    subscribe_msg: Option<String>,
    handler: F,
    cancel_token: CancellationToken,
) -> Result<(), ExchangeError>
where
    F: Fn(String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let (ws_stream, _) = connect_async(url).await
        .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

    let (mut write, mut read) = ws_stream.split();

    // 发送订阅消息
    if let Some(msg) = subscribe_msg {
        write.send(Message::Text(msg)).await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;
    }

    // 处理消息
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handler(text).await;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
```

### 配置结构

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub exchanges: ExchangesConfig,
    pub strategy: StrategyConfig,
    pub risk: RiskConfig,
    pub engine: EngineConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: Option<String>,  // OKX 需要
    pub testnet: bool,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangesConfig {
    pub binance: ExchangeCredentials,
    pub okx: ExchangeCredentials,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    pub funding_arb: FundingArbConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FundingArbConfig {
    pub symbols: Vec<String>,      // ["BTC_USDT", "ETH_USDT"]
    pub min_spread: Decimal,       // 0.0005
    pub max_spread: Decimal,       // 0.002
    pub close_spread: Decimal,     // 0.0002
    pub base_quantity: Decimal,
    pub max_quantity: Decimal,
    pub scaling_mode: String,      // "linear"
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub max_position_value: Decimal,
    pub max_leverage: u32,
    pub max_imbalance_ratio: Decimal,
    pub max_total_exposure: Decimal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    pub queue_capacity: usize,
    pub restart_delay_ms: u64,
    pub max_restart_attempts: u32,
}
```

---

## 快速参考卡片

### Binance 快速参考

| 项目 | 值 |
|-----|-----|
| REST URL | `https://fapi.binance.com` |
| WS Public | `wss://fstream.binance.com/ws` |
| WS Private | `wss://fstream.binance.com/ws/<listenKey>` |
| Symbol 格式 | `BTCUSDT` |
| 签名算法 | HMAC-SHA256 Hex |
| API Key Header | `X-MBX-APIKEY` |
| 订阅格式 | `{"method":"SUBSCRIBE","params":["btcusdt@markPrice"],"id":1}` |
| 费率推送 | `@markPrice` (每3秒) |
| BBO 推送 | `@bookTicker` (实时) |
| ListenKey 有效期 | 60分钟 (需每30分钟续期) |

### OKX 快速参考

| 项目 | 值 |
|-----|-----|
| REST URL | `https://www.okx.com` |
| WS Public | `wss://ws.okx.com:8443/ws/v5/public` |
| WS Private | `wss://ws.okx.com:8443/ws/v5/private` |
| Symbol 格式 | `BTC-USDT-SWAP` |
| 签名算法 | HMAC-SHA256 Base64 |
| 签名内容 | `timestamp + method + path + body` |
| API Key Header | `OK-ACCESS-KEY` |
| 签名 Header | `OK-ACCESS-SIGN` |
| 时间戳 Header | `OK-ACCESS-TIMESTAMP` (ISO 8601) |
| Passphrase Header | `OK-ACCESS-PASSPHRASE` |
| 订阅格式 | `{"op":"subscribe","args":[{"channel":"funding-rate","instId":"BTC-USDT-SWAP"}]}` |
| 费率 Channel | `funding-rate` |
| BBO Channel | `bbo-tbt` |
| 登录格式 | `{"op":"login","args":[{"apiKey":"","passphrase":"","timestamp":"","sign":""}]}` |
| WS 登录签名 | `timestamp + "GET" + "/users/self/verify"` |

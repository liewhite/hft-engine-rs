# 框架总览：模块、接口、怎么用

面向**使用本框架的人**：有哪些模块、它们怎么连、暴露了什么、从零写一个策略到跑起来要做什么。

架构为什么长这样、哪些地方还欠着，见 [`architecture.md`](architecture.md)。

---

## 1. 数据流

```
                    ┌──────────────── 用户代码 (bin) ────────────────┐
                    │  装配交易所 → 起 ManagerActor → 注册策略        │
                    └───────────────────┬───────────────────────────┘
                                        │
   exchange 适配层            ManagerActor（组合根）           strategy 层
  ┌──────────────┐          ┌────────────────────┐          ┌─────────────┐
  │ Binance      │  行情事件 │  装配 / 监督树根    │          │ Strategy    │
  │ OKX          ├─────────►│  停机链根           │          │  trait      │
  │ Hyperliquid  │  账户事件 │  三条总线持有       │          └──────▲──────┘
  │ IBKR         ├─────────►│  快照面应答         │                 │
  └──────▲───────┘          │  ┌──────────────┐  │                 │
         │                  │  │ Provisioner  │  │           StrategyView
         │ REST 下单         │  │ 投产/撤下编排 │  │                 │
         │                  │  └──────┬───────┘  │                 │
         │                  └─────────┼──────────┘                 │
         │                            │ spawn                      │
         │                    ┌───────▼────────┐                   │
         │                    │ ExecutorActor  │───► StrategyRunner ┘
         │                    └───────┬────────┘     (纯逻辑核心)
         │                            │ OutcomeEvent
         │              ┌─────────────┴──────────────┐
         │              │      OutcomePubSub         │
         │              └──────┬──────────────┬──────┘
         │        Outlet::Live │              │ Outlet::Paper
         │         ┌───────────▼──┐    ┌──────▼──────────┐
         └─────────┤OutcomeProc.  │    │ PaperCounter    │
           OrderGW │(唯一实盘出口) │    │ (本地柜台 sim)  │
                   └──────────────┘    └────────┬────────┘
                                                │ 回灌成交回报
                            ┌───────────────────┘
                            ▼
   三条总线 ── MarketPubSub / AccountPubSub / OutcomePubSub
                            │
        ┌───────────────────┼────────────────────┬──────────────┐
        ▼                   ▼                    ▼              ▼
  IncomeProcessor      MetricsActor       PositionLedger    外部观察者
  (按 Delivery 分发)    (观测镜像)         (对账镜像,REST)    (SubscribeXxx)
```

**实盘与模拟并行**，不是两种"运行模式"：两个 outcome 消费者常驻，各按事件自带的
`AccountId` 取自己那份。同一个 symbol 上完全可以模拟先跑、出信号后再拉起实盘。

**回测与实盘共享 `StrategyRunner`**：实盘由 `ExecutorActor` 驱动（actor 串行消费总线），
回测由 `BacktestEngine` 驱动（单线程虚拟时间循环）。策略代码一个字节都不用改。

---

## 2. 模块与依赖

依赖是一张 DAG，箭头指向被依赖方：

```
domain ← messaging ← exchange ← strategy ← engine ← backtest
   ↖________________↙     ↖___ sim ___↙
observability / actor_lifecycle / option：无 crate 内依赖
```

| 模块 | 职责 | 不该做的事 |
|---|---|---|
| `domain` | 值类型与领域规则：`Exchange` / `Order` / `Fill` / `Position` / `AccountId` / `SymbolMeta` / `PriceFormatter` / `ExchangeError` | 不依赖任何其他模块 |
| `messaging` | 事件类型、投递范围、状态投影：`IncomeEvent` / `Delivery` / `SubscriptionScope` / `StateManager` / `PositionBook` / 两条入向总线别名 | 不碰 IO、不碰 actor |
| `exchange` | 四所适配层：WS 订阅、REST 下单、编解码、`CryptoStatusActor` | 不做业务判断 |
| `strategy` | 业务决策，纯函数：`Strategy` trait、`StrategyView`、两个内置策略 | 不碰 IO、不碰 actor、不读墙钟 |
| `engine` | actor 编排：总线、投产、监督、停机、观测、下单出口 | 不写业务判断 |
| `sim` | 撮合状态机与账本：`SimState` / `Ledger` / `Matcher` | 实盘模拟盘回测共用同一份 |
| `backtest` | 历史数据源 + `BacktestEngine` | 不能与实盘走不同的策略实现 |
| `observability` | Prometheus 推送、告警 webhook | 不当权威数据源 |
| `actor_lifecycle` | 停机链：`ChildGroup` / `ChildRegistrar` / `ChildStop` | —— |
| `option` | Black-Scholes 定价（回测合成希腊值用） | —— |

---

## 3. 各模块暴露的接口

### 3.1 `strategy` —— 用户实现的唯一 trait

```rust
pub trait Strategy: Send + Sync {
    /// 要订阅哪些所的哪些数据流。**同时是本策略的可见范围**：
    /// 范围外的 symbol 与交易所，其行情、私有回报、账户读数都不会送达。
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>>;

    /// 挂单多久没有终态就判超时清理
    fn order_timeout_ms(&self) -> u64;

    /// 唯一的事件入口。行情、私有回报、账户读数、券源汇率、Clock 全部经此到达。
    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent>;
}
```

策略能读到的状态就这 9 个方法，读不到的东西编译期就不存在：

| 类型 | 方法 |
|---|---|
| `StrategyView<'_>` | `symbol(&Symbol) -> Option<SymbolView>` / `equity(Exchange)` / `account_notional(Exchange)` / `account_info(Exchange)` / `greeks(Exchange, ccy)` / `market_status(Exchange)` |
| `SymbolView<'_>` | `bbo(Exchange)` / `position(Exchange)` / `position_size(Exchange)` / `position_sizes()` / `has_pending_orders()` / `pending_orders()` |

读数取不到一律 `None`，不返回默认值——净值取不到与净值为零是两回事。

**两个要留意的**：

- `position(ex)` 返回 `Option<&Position>`，`None` = 这条腿**没有记录**（未知）；
  `position_size(ex)` 把未知与空仓都压成 `0.0`。**但 `None` 只在两种情况出现**：
  该所没配凭证（引擎在那儿下不了单），或**模拟策略首笔成交前**（模拟账户不 seed 基线）。
  实盘 + 有凭证的所恒为 `Some`。

  拿它当"停手"判据前先分清自己在哪种情况：对无凭证的所是对的，对模拟策略是陷阱 ——
  会让它永远不下第一单。同一份策略常常既跑实盘又跑模拟，这不是假想情形。
- `market_status(ex)` 是唯一有默认值的读数（默认 `Closed`）。这里的默认是刻意的：
  休市判据只有"要不要下单"一个用途，"不知道开没开"与"确定没开"应导向同一个动作。

产出：

```rust
pub enum OutcomeEvent {
    PlaceOrders { orders: Vec<Order>, comment: String },
    CancelOrder { exchange, symbol, order_id, client_order_id },
    Emit(CustomEvent),   // 发给外部订阅者，下单出口对它 no-op
}
```

### 3.2 `engine` —— 装配与运行时

| 类别 | 接口 |
|---|---|
| 装配 | `setup_binance` / `setup_okx` / `setup_hyperliquid` / `setup_ibkr` → `ExchangeSetup`；`ManagerActorArgs { exchanges, paper }` |
| 投产 | `AddStrategy(StrategySpec)` / `AddStrategies(Vec<StrategySpec>)` / `RemoveStrategies { account, symbols, exchange, flatten }` |
| 账户绑定 | `StrategySpec::live(s)` / `StrategySpec::paper(s, label)` |
| 流面 | `SubscribeMarket` / `SubscribeAccount` / `SubscribeOutcome`，各带 `fn(&Event) -> bool` 过滤器 |
| 快照面 | `GetAllSymbolMetas` / `GetLivePositions` |
| 入向注入 | `PublishCustomEvent(CustomEvent)` |
| 晋升调度 | `SupervisorActor` + `SupervisorArgs`，判据实现 `PromotionPolicy::decide(&SymbolPerformance) -> Decision` |
| 启动辅助 | `init_tracing` / `load_config` / `wait_for_shutdown` / `spawn_supervised` |
| 纯逻辑核心 | `StrategyRunner`（回测直接驱动它）、`ClientOrderIdGen` |

**流面与快照面是两条独立的路**，不要合并成一个 `Query { kind }`：

- **流面**：订阅事件，`fn` 过滤器在发布侧执行，覆盖每一个 `MarketData` / `AccountData` 变体
- **快照面**：一问一答，纯内存读、不打 REST、不阻塞邮箱、由 manager 应答

### 3.3 `exchange` —— 加新交易所的扩展点

```rust
/// 公共数据：无凭证也能用
#[async_trait] pub trait ExchangeClient: Send + Sync + 'static {
    fn exchange(&self) -> Exchange;
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError>;
    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError>;
}

/// 账户私有：只有配了凭证的所才有这个对象
#[async_trait] pub trait AccountClient: Send + Sync + 'static {
    fn exchange(&self) -> Exchange;
    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError>;
    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError>;
    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError>;
    async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError>;
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError>;
}
```

"有没有账户"由 `ExchangeSetup.account: Option<Arc<dyn AccountClient>>` 表达，不另存 bool：
只有 `ExchangeAccess::has_credentials()` 为真时才装进来，拿到 `Some` 即可直接下单。

### 3.4 `messaging` —— 事件与投递

`MarketData` 12 个变体：`BBO` / `MarketTrade` / `MarkPrice` / `IndexPrice` / `FundingRate` /
`Candle` / `HistoryCandles` / `BorrowFee` / `ExchangeRate` / `ExchangeStatus` / `Clock` / `Custom`

`AccountData` 6 个：`OrderUpdate` / `Fill` / `Balance` / `FundingFee` / `Greeks` / `AccountInfo`

投递范围由 `IncomeEvent::delivery()` 推导，三档：

| 档 | 覆盖 | 投给谁 |
|---|---|---|
| `Symbol(所, symbol)` | 行情、symbol 级私有回报 | 订了这个 (所, symbol) 的 |
| `Exchange(所)` | `Balance` / `AccountInfo` / `Greeks` / `ExchangeStatus` / `ExchangeRate` | 订了这个所的 |
| `Broadcast` | `Clock`、无 scope 的自定义事件 | 全部 |

判据只有一份实现（`SubscriptionScope::accepts`），实盘分发层与回测循环共用。

---

## 4. 怎么用

### 4.1 实盘 / 模拟

```rust
use hft_engine_rs::engine::{
    init_tracing, load_config, wait_for_shutdown,
    AddStrategies, ManagerActor, ManagerActorArgs, StrategySpec,
    setup_binance, setup_okx,
};
use kameo::actor::Spawn;
use kameo::mailbox;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    let config: MyConfig = load_config("config.json")?;

    // 1. 装配参与的交易所。缺省即该所不参与 —— manager 对"有哪些所"彻底无知
    let mut exchanges = Vec::new();
    if let Some(a) = config.exchanges.binance { exchanges.push(setup_binance(a)?); }
    if let Some(a) = config.exchanges.okx     { exchanges.push(setup_okx(a)?); }

    // 2. 起 manager：建三条总线、spawn 全部子 actor、链好监督树与停机链
    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs { exchanges, paper: config.paper },
        mailbox::unbounded(),
    );
    manager.wait_for_startup_result().await?;

    // 3. 注册策略。live 走真实下单，paper(label) 落本地柜台，两者可同时跑
    manager.ask(AddStrategies(vec![
        StrategySpec::live(Box::new(my_strategy)),
        StrategySpec::paper(Box::new(my_strategy_2), "BTC"),
    ])).send().await?;

    wait_for_shutdown(&manager).await
}
```

投产是**事务性**的，任一步失败回滚到"什么都没发生"：

```
校验订阅可行性 → 撤本引擎的遗留挂单 → 拉持仓基线（一次快照喂三类消费者）
    → spawn executor（出生即带基线）→ 注册进事件流 → 放行行情订阅
```

"校验订阅可行性"会在**投产期**拒绝：交易所未配置、适配层不支持该 kind、该 symbol 没有
`SymbolMeta`。此时尚无任何副作用，`Err` 即"什么都没发生"——比策略上线后靠"一直没数据"
去猜要好得多。

### 4.2 回测

```rust
use hft_engine_rs::backtest::{BacktestEngine, BinanceHistorySource};
use hft_engine_rs::engine::{SequentialClientOrderIdGen, StrategyRunner};

let runner = StrategyRunner::with_id_gen(
    Box::new(my_strategy),
    symbol_metas.clone(),
    Box::new(SequentialClientOrderIdGen::default()),  // 确定性 id：同输入同输出
);
let mut engine = BacktestEngine::new(source.as_ref(), vec![runner], sim_config, symbol_metas);
let result = engine.run();
// BacktestResult { market_events, fills, realized_pnl, final_equity, initial_balance, positions }
```

数据源实现 `MarketDataSource`；现成的有 `BinanceHistorySource`（含自动下载与本地缓存）、
`TradePrintBboSource`、`BsGreeksSource`（期权合成希腊值）。

回测不读墙钟：`created_at` 取虚拟事件时刻，client_order_id 用自增计数。

### 4.3 外部观察者（导指标、发告警、对接下游）

```rust
use hft_engine_rs::engine::{spawn_supervised, GetLivePositions, SubscribeMarket};

// 只能订阅受监督的 actor —— "订一个没人看着的 actor"在类型上写不出来
let observer = spawn_supervised::<MyObserver>(&manager, args).await?;

manager.tell(SubscribeMarket::new(
    observer,
    |e| matches!(e.data, MarketData::BBO(_)),   // fn 指针，发布侧执行
)).send().await?;

// 快照面：一问一答
let positions: Result<Vec<Position>, String> = manager.ask(GetLivePositions).send().await?;
```

退订：没有显式原语，**订阅者 actor 停机即自动摘除**。受监督的订阅者要收工，
对自己的 `Supervised::actor_ref()` 调 `stop_gracefully()`。

`GetLivePositions` 有两层错误，都要处理，且**任何一层的 `Err` 都不等于"无持仓"**：
mailbox 层的 `Err`（manager 不可达）与业务层的 `Err`（账本不可达）。结果里缺席的腿是
"未知"而非"空仓"，不要为它报指标。

---

## 5. 扩展点

| 要加什么 | 做什么 | 要不要动框架 |
|---|---|---|
| 新策略 | 实现 `Strategy` | 否 |
| 新交易所 | 实现 `ExchangeClient` + `AccountClient`，写一个 `setup_xxx` | 否，manager 零改动 |
| 新数据源（回测）| 实现 `MarketDataSource` | 否 |
| 新晋升 / 降级判据 | 实现 `PromotionPolicy` | 否 |
| 新事件类型（用户域）| `CustomEvent` + `PublishCustomEvent` / `OutcomeEvent::Emit` | 否，框架只运送不理解 |
| 新事件变体（框架域）| 给 `MarketData` / `AccountData` 加变体 | 是，且所有 `match` 会**编译失败**，漏改不了 |

---

## 6. 几个容易踩的地方

- **`public_streams()` 同时是可见范围**。它决定了策略能收到哪些所的账户读数——
  想读某所的净值，就得订它的公共流。
- **`StrategyView` 不能跨事件持有**。生命周期钉死在一次 `on_event` 调用内，
  存起来会读到陈旧状态（编译器会拦）。
- **持仓靠「一次 REST 基线 + Fill 永久累加」维护**，不是每次查 REST。
  对账层（`PositionLedgerActor`）持续用 REST 校验，确认漂移后**致命退出**。
- **未 seed 的腿是"未知"不是"空仓"**。没有 `AccountClient` 的所不 seed，
  但引擎在那里也下不了单，所以 `position_size` 报 0 是事实。
- **停机分三段**：生产者 → 总线 → 消费者。新增消费者时先问它会不会往回灌事件
  （见 architecture.md V5，那条残余窗口是已知并接受的）。

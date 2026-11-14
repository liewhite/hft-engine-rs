# 框架总览：模块、接口、怎么用

面向**使用本框架的人**：有哪些模块、它们怎么连、暴露了什么、从零写一个策略到跑起来要做什么。

架构为什么长这样、哪些地方还欠着，见 [`architecture.md`](architecture.md)。

---

## 1. 数据流

```
                    ┌──────────────── 用户代码 (bin) ────────────────┐
                    │  装配交易所 → run_engine 起 Manager → 注册策略  │
                    └───────────────────┬───────────────────────────┘
                                        │
   exchange 适配层            Manager（组合根）                strategy 层
  ┌──────────────┐          ┌────────────────────┐          ┌─────────────┐
  │ Binance      │  行情事件 │  装配               │          │ Strategy    │
  │ OKX          ├─────────►│  生产者/消费者两组   │          │  trait      │
  │ Hyperliquid  ├─────────►│  三条总线的建造者   │          └──────▲──────┘
  │              │  账户事件 │  快照面应答         │                 │
  └──────▲───────┘          │  ┌──────────────┐  │                 │
         │                  │  │ Provisioning │  │           StrategyView
         │ REST 下单         │  │ 投产凭证      │  │                 │
         │                  │  └──────┬───────┘  │                 │
         │                  └─────────┼──────────┘                 │
         │                            │ spawn                      │
         │                    ┌───────▼────────┐                   │
         │                    │ Executor       │───► StrategyRunner ┘
         │                    └───────┬────────┘     (纯逻辑核心)
         │                            │ OutcomeEvent
         │              ┌─────────────┴──────────────┐
         │              │      OutcomeBus            │
         │              └──────┬──────────────┬──────┘
         │        Outlet::Live │              │ Outlet::Paper
         │         ┌───────────▼──┐    ┌──────▼──────────┐
         └─────────┤OutcomeProc.  │    │ PaperCounter    │
           OrderGW │(唯一实盘出口) │    │ (本地柜台 sim)  │
                   └──────────────┘    └────────┬────────┘
                                                │ 回灌成交回报
                            ┌───────────────────┘
                            ▼
   三条总线 ── MarketBus / AccountBus / OutcomeBus
                            │
        ┌───────────────────┼────────────────────┬──────────────┐
        ▼                   ▼                    ▼              ▼
  IncomeProcessor      Metrics            PositionLedger    外部观察者
  (按 Delivery 分发)    (观测镜像)         (对账镜像,REST)    (inbox.subscribe)
```

**实盘与模拟并行**，不是两种"运行模式"：配了模拟柜台（`ManagerArgs::paper`）时两个
outcome 消费者并存，各按事件自带的 `AccountId` 取自己那份。同一个 symbol 上完全可以模拟先跑、
出信号后再拉起实盘。只跑实盘的进程 `paper: None`，不起模拟柜台；此时绑定模拟账户的策略投产被拒。

**连接与账户轮询按需起**（#757）：各所公共 WS 在第一条订阅路由到它时才连；账户类数据流
（如 OKX 希腊值，见 `AccountStream`）由策略的 `account_streams()` 声明，投产时才起。

**回测与实盘共享 `StrategyRunner`**：实盘由 `Executor` 驱动（一个任务串行消费自己的收件箱），
回测由 `BacktestEngine` 驱动（单线程虚拟时间循环）。策略代码一个字节都不用改。

---

## 2. 模块与依赖

依赖是一张 DAG，箭头指向被依赖方：

```
domain ← messaging ← exchange ← strategy ← engine ← backtest
   ↖________________↙     ↖___ sim ___↙
observability / tasks / option：无 crate 内依赖（messaging 起各层都用 tasks）
```

| 模块 | 职责 | 不该做的事 |
|---|---|---|
| `domain` | 值类型与领域规则：`Exchange` / `Order` / `Fill` / `Position` / `AccountId` / `SymbolMeta` / `PriceFormatter` / `ExchangeError` | 不依赖任何其他模块 |
| `messaging` | 事件类型、投递范围、状态投影：`IncomeEvent` / `Delivery` / `SubscriptionScope` / `StateManager` / `PositionBook` / 两条入向总线别名 | 不碰 IO、不起任务 |
| `exchange` | 四所适配层：各所节点 `XxxNode`（WS 订阅、轮询）、REST 下单、编解码、`CryptoStatus` | 不做业务判断，不依赖 engine |
| `strategy` | 业务决策，纯函数：`Strategy` trait、`StrategyView`、两个内置策略 | 不碰 IO、不起任务、不读墙钟 |
| `engine` | 组件编排：总线、投产、停机、观测、下单出口 | 不写业务判断 |
| `sim` | 撮合状态机与账本：`SimState` / `Ledger` / `Matcher` | 实盘模拟盘回测共用同一份 |
| `backtest` | 历史数据源 + `BacktestEngine` | 不能与实盘走不同的策略实现 |
| `observability` | Prometheus 推送、告警 webhook | 不当权威数据源 |
| `tasks` | 组件运行时：任务树（`Root` / `Group` / `Scope` / `Task`，生命周期与出错上冒）、收件箱（`Inbox` / `Addr`）、总线（`Bus`）、`spawn_addressed` / `spawn_periodic` | 不含业务 |
| `option` | Black-Scholes 定价（回测合成希腊值用） | —— |

---

## 3. 各模块暴露的接口

### 3.1 `strategy` —— 用户实现的唯一 trait

```rust
pub trait Strategy: Send + Sync {
    /// 要订阅哪些所的哪些数据流。**同时是本策略的可见范围**：
    /// 范围外的 symbol 与交易所，其行情、私有回报、账户读数都不会送达。
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>>;

    /// 唯一的事件入口。行情、私有回报、账户读数、券源汇率、Clock 全部经此到达。
    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<Signal>;
}
```

策略能读到的状态就这几个方法，读不到的东西编译期就不存在：

| 类型 | 方法 |
|---|---|
| `StrategyView<'_>` | `symbol(&Symbol) -> Option<SymbolView>` / `equity(Exchange)` / `account_notional(Exchange)` / `account_info(Exchange)` / `greeks(Exchange, ccy)` |
| `SymbolView<'_>` | `bbo(Exchange)` / `position_size(Exchange)` / `position_sizes()` / `has_pending_orders()` / `pending_orders()` |

读数取不到一律 `None`，不返回默认值——净值取不到与净值为零是两回事。

**要留意的**：**持仓没有"未查询到"这个状态**。作用域内每条 (所, symbol) 在策略消费第一条事件之前
  就已写入基线（executor 出生时由投产握手写满），所以 `position_size()` 返回的 `0.0`
  永远是"确定空仓"。策略不必、也无法检查"这条腿查到了没有"。

产出：

```rust
pub enum Signal {                    // 策略产出
    PlaceOrders { orders: Vec<OrderIntent>, comment: String },
    CancelOrder { symbol, client_order_id },   // 所由框架从跟踪表查
}
pub enum OutcomeEvent {              // StrategyRunner 处理后发往出口
    PlaceOrders { orders: Vec<Order>, comment: String },
    CancelOrder { exchange, symbol, client_order_id },
}
```

策略只给下单意图 `OrderIntent`（所、币、方向、类型、数量、只减仓），没有订单号、不必取整。
`StrategyRunner` 是**下单范围的唯一闸门**：(所, 币) 不在本策略订阅范围内、没有合约规格或该所没装配，
整机停机；否则分配客户端订单号、按交易所精度取整成 `Order`，登记进订单跟踪表的就是这张发出的单。
撤单同样只能撤范围内的币，撤哪个所取自跟踪表。

### 3.2 `engine` —— 装配与运行时

| 类别 | 接口 |
|---|---|
| 装配 | 配置里 `exchanges` 段解析成 `ExchangesConfig`，`setup_all(&cfg)` → `Vec<ExchangeSetup>`；`ManagerArgs { exchanges, paper: Option<SimConfig> }` |
| 投产（`Provisioning`，`Manager::spawn` / `run_engine` 交出唯一一张，用一次就没了） | `add_strategy(spec)` / `add_strategies(specs)`，`.await -> Result<(), ExchangeError>` |
| 账户绑定 | `StrategySpec::live(s)` / `StrategySpec::paper(s, label)` |
| 流面 | `market_bus()` / `account_bus()` / `outcome_bus()`，观察者在构造时 `inbox.subscribe(bus, fn(&Event) -> bool, Msg::X)` |
| 挂观察者 | `observers()`：manager 的消费者组，`spawn` / `spawn_addressed` / `spawn_periodic` |
| 快照面 | `exchanges()` / `all_symbol_metas()` / `symbol_meta(ex, &sym)`（同步）；持仓不走快照面，订账户总线上持仓账本发的 `PositionValues` |
| 各所公共能力 | `settled_funding(ex)`：已结算资金费率来源（用装配时建好的 client，不另建） |
| 入向注入 | `publish_custom_event(CustomEvent)`（同步，直接发行情总线） |
| 启动辅助 | `run_main`（日志与告警：启动试发、退出前发完）/ `load_config` / `run_engine(args, \|handle, provisioning\| ..)`（启动 → 装配 → 运行 → 唯一停机路径） |
| 纯逻辑核心 | `StrategyRunner`（回测直接驱动它）、`ClientOrderIdGen` |

**流面与快照面是两条独立的路**，不要合并成一个 `Query { kind }`：

- **流面**：订阅事件，`fn` 过滤器在发布侧执行，覆盖每一个 `MarketData` / `AccountData` 变体
- **快照面**：纯内存读、不打 REST（品种目录直接读句柄里那份）

### 3.3 `exchange` —— 加新交易所的扩展点

```rust
/// 公共数据：无凭证也能用
#[async_trait] pub trait ExchangeClient: Send + Sync + 'static {
    fn exchange(&self) -> Exchange;
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError>;
}

/// 账户私有：只有配了凭证的所才有这个对象
#[async_trait] pub trait AccountClient: Send + Sync + 'static {
    fn exchange(&self) -> Exchange;
    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, PlaceError>;
    async fn cancel_order(&self, symbol: &Symbol, client_order_id: &str) -> Result<(), ExchangeError>;
    /// 账户上全部挂单（启动清场用，不限本引擎接入的币）及按交易所订单号撤
    async fn fetch_open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError>;
    async fn cancel_open_order(&self, order: &OpenOrder) -> Result<(), ExchangeError>;
    /// 净值（带币种：本所计价币那一份，不折算）
    async fn fetch_equity(&self) -> Result<Money, ExchangeError>;
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError>;
}
```

"有没有账户"由 `ExchangeSetup.account: Option<Arc<dyn AccountClient>>` 表达，不另存 bool：
各所的公共接口与账户接口是两个类型（`BinanceClient` / `BinanceAccount`、`OkxClient` / `OkxAccount`、
`HyperliquidClient` / `HyperliquidAccount`），账户类型的凭证是必填字段，只在配了凭证时由 `setup_*`
构造（#813）；拿到 `Some` 即可直接下单。签名请求每所只有一处（`BinanceAccount::signed`、
`OkxAccount::signed`、HL 的签名通道 `HlSubmitter`）。

### 3.4 `messaging` —— 事件与投递

`MarketData` 10 个变体：`BBO` / `MarketTrade` / `MarkPrice` / `IndexPrice` / `FundingRate` /
`Candle` / `HistoryCandles` / `ExchangeStatus` / `Clock` / `Custom`

`AccountData` 按定位精度在类型上分两档（#696）：
- `Symbol(SymbolAccountData)`：`OrderUpdate` / `Fill` / `FundingFee`
- `Exchange(ExchangeAccountData)`：`Balance` / `Greeks` / `Equity` / `PositionValues`（品种目录外的成交适配层直接丢弃，不上报）

状态层按档分派、不带默认分支；`SymbolState` 只收币级。新增账户事件先选档，漏处理编译不过。

投递范围由 `IncomeEvent::delivery()` 推导（行情每个变体自己声明档位，账户事件由类型分档给出），三档：

| 档 | 覆盖 | 投给谁 |
|---|---|---|
| `Symbol(所, symbol)` | 行情、symbol 级私有回报 | 订了这个 (所, symbol) 的 |
| `Exchange(所)` | `ExchangeAccountData` 全部 / `ExchangeStatus` | 订了这个所的 |
| `Broadcast` | `Clock`、无 scope 的自定义事件 | 全部 |

判据只有一份实现（`SubscriptionScope::accepts`），实盘分发层与回测循环共用。订阅范围带绑定账户：
账户事件先看归属（别的账户的私有事件不收），再看上表。

---

## 4. 怎么用

### 4.1 实盘 / 模拟

```rust
use hft_engine_rs::engine::{
    load_config, run_engine, run_main,
    ManagerArgs, StrategySpec, setup_all,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_main(run).await
}

async fn run() -> anyhow::Result<()> {
    let config: MyConfig = load_config("config.json")?;

    // 1. 装配参与的交易所（config.exchanges: ExchangesConfig）。缺省即该所不参与 ——
    //    manager 对"有哪些所"彻底无知
    let exchanges = setup_all(&config.exchanges).await?;

    // 2. run_engine 建任务树、起 manager（建三条总线、在生产者/消费者两组下起全部子任务；
    //    返回前各所私有推送都已确认订阅生效），再跑装配闭包，然后等停机信号或任何节点出错。
    //    闭包出错也先停机再返回 —— 装配到一半的引擎不会被丢下不管。
    run_engine(ManagerArgs { exchanges, paper: Some(config.paper) }, |manager, provisioning| async move {
        // 3. 注册策略。live 走真实下单，paper(label) 落本地柜台，两者可同时跑
        provisioning.add_strategies(vec![
            StrategySpec::live(Box::new(my_strategy)),
            StrategySpec::paper(Box::new(my_strategy_2), "BTC"),
        ]).await?;
        Ok(())
    })
    .await
}
```

一个进程**只在启动时投产一次**：投产方法拿走凭证，第二次投产的代码编译不过（#826）；投产失败返回
`Err`，同时整机停机（半起的实例不回滚，随整机收掉）：

```
校验订阅可行性 → 拉持仓基线（一次快照喂三类消费者）→ 注册对账/观测镜像
    → spawn executor（出生即带基线）→ 注册进事件流 → 放行行情订阅
```

（账户上的遗留挂单不在投产时撤，而是引擎启动时一次撤净整个账户，见 `manager::clearing`。）

"校验订阅可行性"会在**投产期**拒绝：交易所未配置、适配层不支持该 kind、该 symbol 没有
`SymbolMeta`——比策略上线后靠"一直没数据"去猜要好得多。

### 4.2 回测

```rust
use hft_engine_rs::backtest::{BacktestEngine, BinanceHistorySource};
use hft_engine_rs::engine::{SequentialClientOrderIdGen, StrategyRunner};

let runner = StrategyRunner::new(
    Box::new(my_strategy),
    symbol_metas.clone(),
    hft_engine_rs::backtest::backtest_account(),       // 回测账户：只收它的私有事件
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

观察者挂在 manager 的**消费者组**（`manager.observers()`）上：与引擎内部的消费者同一批停、
在全部生产者之后，生产者收尾时补发的最后一批事件照样收得到；它出错（`run` 返回 `Err` 或
panic）整机受控停机。

```rust
use hft_engine_rs::tasks::{Inbox, Scope, Task};

struct MyObserver { inbox: Inbox<MarketEvent> }

impl Task for MyObserver {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        // recv 返回 None = 已被要求停止、收件箱已排空
        while let Some(ev) = self.inbox.recv().await { /* ... */ }
        Ok(())
    }
}

let bus = manager.market_bus().clone();
let observer = manager
    .observers()
    .spawn("MyObserver", move |scope: Scope| async move {
        // 收件箱只能从树上的 Scope 建：能订阅的一定是有人监督的组件
        let (_addr, mut inbox) = scope.inbox::<MarketEvent>();
        inbox.subscribe(&bus, |e| matches!(e.data, MarketData::Symbol(SymbolMarketData::BBO(_))), |e| e); // fn 指针，发布侧执行
        Ok(MyObserver { inbox })
    })
    .await?;

// 定时轮询（首拍立即、慢了不积压补跑，tick 返回 Err 整机停）
manager.observers().spawn_periodic("MyPoller", Duration::from_secs(60), my_periodic)?;

// 持仓：订持仓账本发的读数（与策略风控闸门同一个数，见 external-data-access.md §三）
inbox.subscribe(manager.account_bus(), |e| e.account == AccountId::Live
    && matches!(e.data, AccountData::Exchange(ExchangeAccountData::PositionValues(_))), Msg::Positions);
```

退订：没有显式原语，**订阅者停机即自动摘除**（订阅凭证存在收件箱里，收件箱关闭即退订）。
观察者要收工，对 `spawn` 返回的 `ChildHandle` 调 `stop().await`，只收掉它自己。

读数里缺席的腿是"未知"而非"空仓"，不要为它报指标。

---

## 5. 扩展点

| 要加什么 | 做什么 | 要不要动框架 |
|---|---|---|
| 新策略 | 实现 `Strategy` | 否 |
| 新交易所 | 实现 `ExchangeClient` + `AccountClient`，写一个 `setup_xxx` | 否，manager 零改动 |
| 新数据源（回测）| 实现 `MarketDataSource` | 否 |
| 新事件类型（用户域）| `CustomEvent` + `publish_custom_event`（外部向策略） | 否，框架只运送不理解 |
| 新事件变体（框架域）| 选档（币级 / 所级）后给 `SymbolMarketData` / `ExchangeMarketData` / `SymbolAccountData` / `ExchangeAccountData` 加变体 | 是，且所有 `match` 会**编译失败**，漏改不了 |

---

## 6. 几个容易踩的地方

- **`public_streams()` 同时是可见范围**。它决定了策略能收到哪些所的账户读数——
  想读某所的净值，就得订它的公共流。
- **`StrategyView` 不能跨事件持有**。生命周期钉死在一次 `on_event` 调用内，
  存起来会读到陈旧状态（编译器会拦）。
- **持仓靠「一次 REST 基线 + Fill 永久累加」维护**，不是每次查 REST。
  对账层（`PositionLedger`）持续用 REST 校验，确认漂移后**致命退出**。
- **持仓在策略看到第一条事件前就已就绪**。三种来源：有凭证的所用 REST 快照（账户真实
  持仓），无凭证的所补 0（本引擎在那儿下不了单，恒为 0），模拟账户补 0（从零起步）。
- **停机分两组**：生产者 → 消费者（总线是普通值，不占停机段）。新增消费者时先问它会不会
  往回灌事件（见 architecture.md V5，那条残余窗口是已知并接受的）。

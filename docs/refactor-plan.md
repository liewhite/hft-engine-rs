# 引擎核心激进重构计划

> 2026-08-11 制定。原则：SSOT / 实体最少化 / 无特殊逻辑。不考虑向后兼容。
> 对照基准：六环节目标架构（交易所 trait → 聚合分发 → 纯函数策略 → 决策出口 → 虚拟柜台 → 外部订阅）。
>
> **执行状态（2026-08-11，分支 refactor/stage1-counter-validation）：阶段 1–5 全部完成。**
>
> | 阶段 | commit | 审查 |
> |---|---|---|
> | 1 柜台校验收敛 | 762bb6f | 与阶段 2 合并审查：1 Critical（place 假成功）已修（76c49a5，PlaceVerdict + 契约直测） |
> | 2 单一下单出口 | 73a2c1b | 同上；改进：共享 OrderGateway 组件替代计划中的 Execute ask（不占 outcome 邮箱） |
> | 3 基线退出总线 | ef0ec81 | 0 Critical；文档诚实性修正 + 补 2 测试（230cecd） |
> | 4 事件模型拆分 | f6dbfeb | 0 Critical、2 Major（注册幂等/丢弃点可见）已修（d4edbce） |
> | 5 Manager 瘦身 | b0237f7 | （审查中） |
>
> 与计划的偏离（均为改进，理由见各 commit）：
> - 阶段 2 用共享 `OrderGateway` 组件替代 `Execute` ask 消息 —— 平仓 REST 不阻塞 outcome 邮箱；
> - 阶段 3 顺带把对账盲区收窄（executor 与镜像 seed 语义同构），残余窗口在
>   `activate_executors` 文档如实列明；
> - 阶段 4 顺带完成 B5（路由索引化）与 Supervisor 账户侧单订阅；
> - Greeks 归入 AccountData 零行为变化（见事实核查：paper gamma_scalp 原本就因缺 cashBal 不可用）。
> 阶段 6（conflation / 并行停机 / 仓库清理）未动，按需另排。

## 0. 诊断归因

上一轮审查发现的问题，按根因收敛为两个上游错误 + 两处知识散落：

1. **事件的账户归属靠推断**（"共享总线上的私有事件必属 Live"）+ 一张手工分类表
   `is_account_private()`。下游症状：Paper/Live 私有事件形状不对称（`AccountIncome`
   vs 裸 `IncomeEvent`）、新增变体漏改分类表时私有事件会广播给模拟策略（危险侧静默）、
   Greeks 已经漏在表外。
2. **持仓基线混进广播总线**。基线本质是"只给特定消费者、只发一次"的握手数据，放上
   pub/sub 就必须造例外：`EventRouting::SkipExecutors`、`baselined_positions` 去重集、
   对账层 `pending_fills` 缓冲重放，且 executor 的双计窗口不在对账覆盖内
   （镜像与 executor 收基线时序不同构）。
3. **下单知识两处**：`OutcomeProcessorActor`（in_flight / 回报合成 / dry_run）与
   `ManagerActor::RemoveStrategies` 的平仓直发 `client.place_order`（三样全无）。
4. **市场规则校验散在驱动层**：下界校验实盘在 `ExchangeOrder::from_domain`、模拟盘在
   `PaperCounterActor`，回测路径缺失（`SimState` 不持 metas）——实盘必拒的单回测照常成交。

## 1. 目标态

### 1.1 事件模型：分类是结构，不是推断

```rust
/// 公共事件：无账户归属，一份服务所有账户
struct MarketEvent { exchange_ts, local_ts, data: MarketData }
enum MarketData {
    BBO | MarketTrade | MarkPrice | IndexPrice | FundingRate
  | Candle | HistoryCandles | ExchangeStatus
  | BorrowFee | ExchangeRate            // 参考数据，无账户
  | Clock | Custom(CustomEvent)
}

/// 账户私有事件：账户是必填结构字段，不可能"忘了归属"
struct AccountEvent { account: AccountId, exchange_ts, local_ts, data: AccountData }
enum AccountData {
    OrderUpdate | Fill | Balance | AccountInfo | FundingFee
  | Greeks    // 显式归入账户侧（现状漏在 is_account_private 表外、被广播给模拟策略）
}

/// 策略与状态层的统一视图
enum IncomeEvent { Market(MarketEvent), Account(AccountEvent) }
```

- `PositionBaseline` / `PositionReport` **不再是事件**（见 1.3）。
- 路由完全由事件形状推导：Market 按 scope（有 symbol 定向 / 无 symbol 广播），
  Account 按 (account, symbol)。`EventRouting` 枚举整个删除，零例外。

### 1.2 总线：三条，职责对称

```
MarketBus  : 各所公共 WS/轮询 ──→ 所有订阅者（一份行情服务所有账户）
AccountBus : 各所私有 WS(构造期注入 AccountId::Live)
             + PaperCounter(Paper(x)) ──→ 按账户路由
OutcomeBus : executor ──→ 两种柜台按账户认领（实盘出口 / 本地柜台），维持现状
```

- `PaperPubSub` 与 `AccountIncome` 删除：模拟与实盘的私有事件是同一个类型、同一条总线，
  只是 `account` 字段不同。"实盘与模拟只是两种柜台"从文档约定变成类型结构。
- 适配层私有路径在 actor 构造时注入账户标签——将来多实盘账户只需放开这个注入点，
  不改事件模型（现在**不**建多账户配置面，奥卡姆）。

### 1.3 基线与对账：握手数据，不上总线

- `StateManager::seed(positions, snapshot_req_ts)`：唯一的基线入口，只可调一次
  （二次调用即违约，保留现有 error 语义）。内置统一防重规则：seed 后丢弃
  `local_ts <= snapshot_req_ts` 的 Fill（该成交必已含在快照里）。残余窗口 = REST 在途
  时长，**全部消费者同一条论证、文档只写一处**——镜像与 executor 时序同构，
  现状"executor 自身偏差不在对账覆盖内"的盲区随之消失。
- **executor**：`ExecutorArgs` 直接携带 baselines——先拉快照再 spawn，出生即带基线，
  不存在任何投递窗口（比现状"注册前点对点 tell"更强）。
- **镜像**（reconciler / metrics）：`RegisterSymbols` 改为携带 baselines 的注册握手，
  注册与基线原子到达，`pending_fills` 缓冲重放机制整体删除。
- **对账读数**：`PositionPollingActor` 并入 `PositionReconcileActor`——reconciler 持有
  authed clients，由 Clock 驱动自行轮询比对。读数只有一个消费者，不该经过总线。

### 1.4 出向：一条出口、一处规则

- **市场规则（下界 + 精度）**：`SymbolMeta::checked_exchange_qty` 仍是唯一规则出处，
  校验点收敛到**柜台**——`SimState::on_order_arrived`（回测与模拟盘共享，拒单走既有
  `Rejected` 回报路径）与 `ExchangeOrder::from_domain`（实盘线路边界）。
  `PaperCounterActor` 的预校验删除。三驱动口径自动一致。
- **下单出口**：`OutcomeProcessorActor` 内部提炼 `async fn dispatch(...)`，
  同时服务两个入口——总线订阅（现状，spawn 不阻塞）与新增 `ask` 消息
  `Execute(AccountOutcome) -> Result`（同步等结果）。Manager 降级平仓改走 `Execute`，
  自动获得 in_flight 假警报防护、回报合成、dry_run 语义。系统从此只有一个组件知道
  "怎么把订单发上交易所"。

### 1.5 装配：Manager 只做监督根

- `setup_*` 迁至各交易所模块；`ManagerActorArgs { exchanges: Vec<ExchangeSetup>, paper }`，
  bin 侧组装。**加新交易所 = 新增 exchange 模块 + bin 里一行**，manager 零改动。
- `supports_subscription` 的集中 match 删除，能力知识下放各适配层。
  **实做偏离**：不是 `ExchangeActorOps::supports(kind)`（trait 方法要 actor 实例），而是
  各所模块的 `pub fn supports_subscription` + `ExchangeSetup.supports` fn 指针 —— 能力是
  适配层的静态属性，测试无需 spawn actor 即可用生产同款函数。不要按原计划"改回去"。
- `manager.rs` 拆为 `assembly` / `provisioning` / `api` 子模块。**不新增 actor**：
  投产编排必须经同一邮箱串行化，这正是 actor 的用途；瘦的是文件与职责边界，不是拆邮箱。

### 1.6 实体增删清单

**删除**（机制连同其测试负担）：

| 现状实体/机制 | 归宿 |
|---|---|
| `PaperPubSub`、`AccountIncome` | 并入 AccountBus / `AccountEvent` |
| `is_account_private()` 分类表 | 类型结构替代 |
| `ExchangeEventData::PositionBaseline` | 注册握手载荷 |
| `ExchangeEventData::PositionReport` | reconciler 内部直查 |
| `EventRouting`（含 SkipExecutors） | 路由由事件形状推导 |
| `ManagerActor::baselined_positions` | 不再需要 |
| `Reconciler::pending_fills` 缓冲重放 | 不再需要 |
| `PositionPollingActor` | 并入 reconciler |
| Manager 内平仓下单代码 | `Execute` 单一出口 |
| `PaperCounterActor` 下界预校验 | `SimState` 统一校验 |
| `supports_subscription` 集中 match | `ExchangeActorOps::supports` |
| `ManagerActorArgs` 四个具名所字段 | `Vec<ExchangeSetup>` |

**新增**（仅三个）：Market/Account 枚举拆分本身、`Execute` 消息、`StateManager::seed`。

## 2. 分阶段执行

每阶段独立编译、测试全绿、小步 commit；阶段完成后跑 `code-reviewer` subagent。

### 阶段 1：柜台校验收敛（~0.5 天）
`SimState` 构造收 `Arc<HashMap<Symbol, SymbolMeta>>`；`on_order_arrived` 用
`checked_exchange_qty` 校验，不合法走既有 `Rejected` 路径。`BacktestEngine` 构造传
metas；删 `paper_counter.rs` 预校验。
**验收**：新增"低于最小下单量的单在回测中被拒"测试；既有 sim/backtest 测试全绿。

### 阶段 2：单一下单出口（~0.5 天）
`OutcomeProcessorActor` 提炼 `dispatch_place` / `dispatch_cancel`；新增 `Execute` ask 消息；
`RemoveStrategies` 平仓改经 `Execute`；删 manager 内 wire 构造与直发。
**验收**：既有撤单复查/in_flight 测试全绿；平仓失败仍如实计入 `incomplete`。

### 阶段 3：基线与对账读数退出总线（1–2 天）
按 1.3 实施。删两个事件变体、`SkipExecutors`、`baselined_positions`、`pending_fills`、
`PositionPollingActor`。
**测试移植**：`position_report_is_never_routed_to_executors` 等守恒测试改写为
"seed 只可调一次" / "seed 后旧 Fill 被过滤"；缓冲重放测试删除。
**顺序依据**：先把两个畸形变体清出枚举，阶段 4 的拆分才干净。

### 阶段 4：事件模型拆分 + 总线重组 + 路由索引化（2–4 天，最大一刀）
- `messaging` 重写为 1.1 三类型；`Strategy::on_event(&IncomeEvent, ..)` 签名保持、
  内部改嵌套 match（spread_arb / gamma_scalp 机械迁移，编译器穷尽检查驱动）。
- 适配层：public 路径发 `MarketEvent`；private 路径发 `AccountEvent{Live}`（构造期注入）。
- `PaperCounterActor` 发 `AccountEvent{Paper(x)}` 到同一条 AccountBus。
- `IncomeProcessorActor`：两个 handler；建 `(Exchange,Symbol) → Vec<ExecutorRef>` 与
  `AccountId → Vec<ExecutorRef>` 反向索引，热路径 O(1)（替代现在每条事件线性扫描全部
  executor）。
- Supervisor：一次订阅 AccountBus（替代现状的共享总线 + Paper 总线双订阅）+ Clock。
- Backtest：每个 runner 绑定账户，SimState 回报按账户定向投递——顺带修掉多 runner
  同 symbol 时互相吃到对方成交回报的问题。
- **Greeks 归入 AccountData，零行为变化**（已核实）：Greeks 是引擎外期权腿的读数
  （实盘 = OKX 账户里手工买入的期权；回测 = `BsGreeksSource` 按配置合成），不是柜台
  持仓的派生值——虚拟柜台只有永续持仓，无从计算。且现状模拟账户的 gamma_scalp 本就
  不可用：`StateManager::greeks()` 要求 greeks + cashBal 双到齐，而 Balance 是私有
  事件只投 Live，paper executor 恒缺 cashBal → 恒早退。现状广播无任何受益者。
  模拟盘将来要跑 gamma_scalp 时按需另建（不进本轮）：
  - 全虚拟期权腿（默认）：实盘版 BsGreeks 合成 actor，听真实成交、按虚拟跨式发
    Greeks + Balance 到 paper 账户；BS 聚合逻辑与 `bs_greeks_source.rs` 提纯共享（SSOT）。
  - 真持有期权、只模拟对冲腿：Manager 加 `PublishAccountEvent` 入向口（与
    `PublishCustomEvent` 对称），外部显式桥接 Live greeks + cashBal 到 paper 账户。
**验收**：account isolation / routing 守恒测试移植到新类型；四所联网 ignored 测试手动跑通。

### 阶段 5：Manager 瘦身与装配收敛（~1 天）
按 1.5 实施。无行为变化，纯结构迁移。
**验收**：各 bin 启动路径不变；manager 拆为 mod.rs（823 行，含订阅门面与 90 行测试）+
provisioning.rs —— 原定"< 500 行 / 三个子模块"改为两文件方案：订阅门面消息与
"总线持有者"职责同体，再拆第三个文件只挪行数不清边界。
**后续可选**：三个 bin 的装配块（`if let Some → push` × 3 所）与同构的 `ExchangesConfig`
可提为 assembly.rs 里的 `ExchangesConfig::assemble()`，加新加密所时不必改三个 bin。

### 阶段 6：后续（另行排期，不在本轮）
- MarketBus conflation：状态类事件（BBO/Mark/AccountInfo）最新值语义 + 合并计数指标，
  替代"unbounded 内存涨 vs bounded 静默丢"的二选一。
- `ChildGroup` 组内并行停机（保组间顺序，超时者单独点名）。
- 仓库清理：`AWSCLIV2.pkg`（52MB）、根目录散落 python 脚本出库。

## 3. 有意不做（奥卡姆）

- **多实盘账户**：只留适配层的账户注入点，不建配置面——无现实需求。
- **`Exchange` 枚举换注册表**：4 所规模下，封闭枚举的穷尽 match 安全性更值钱。
- **actor 框架 / 总线替换**：kameo + pubsub 工作良好，问题全在其上的使用方式。

## 4. 风险与对策

- 阶段 4 波及面最大（全部 codec 发布点、两份策略、backtest、metrics、supervisor）。
  对策：全程靠封闭枚举的穷尽检查驱动，编译器保证不漏；该阶段期间不并行其他改动。
- 停机链 / 监督语义不动（`actor_lifecycle` 原样保留），阶段 3/4 中所有 actor 增删都
  沿用 `ChildGroup` 既有约定。
- 联网 ignored 测试（四所 order test、`public_stream_noauth`）在阶段 3、4 完成后各手动
  跑一轮，作为上线前验收。

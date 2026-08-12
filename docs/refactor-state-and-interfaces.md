# 重构计划：状态投影与接口收窄

针对 [architecture.md](./architecture.md) §4 的违反清单 V1–V4、V6、V7。判据、原则、品味都在那份
文档里，这里只写**做什么、怎么验收、有什么风险**。

上一轮重构（Manager 瘦身 / 事件模型拆分 / 持仓基线退出总线）的记录见
[refactor-plan.md](./refactor-plan.md)，本轮是它的延续：上一轮解决了"谁跟谁说话"，
这一轮解决"说话时递过去多少东西"。

## 状态

| 阶段 | 目标 | 对应违反 | 状态 |
|---|---|---|---|
| R1 | 拆 `ExchangeClient`：公共 / 账户两个 trait | V3 | **已完成** |
| R2 | 拆 `StateManager`：四个单一职责投影 | V1 | **已完成** |
| R3 | 策略只拿受限视图 | V2 | **已完成** |
| R4 | 下单出口的分发判据收敛到一处 | V4 | **已完成** |
| R5 | 收尾：投产编排独立、观测口径移出领域层 | V6 / V7 | **已完成** |

顺序理由：R1 独立性最好（只动适配层与装配），先做能立刻消掉一类"空值当事实"的补丁；
R2 是 R3 的前置，也是全局风险最高的一步；R4 独立且小，可以随时插队。

---

## R1 拆 `ExchangeClient`

### 现状

一个 8 方法 trait 混装公共行情与账户私有能力。无凭证的所把"没有账户"表达成返回空值：

```rust
// binance/client.rs:706 / okx/client.rs:599 / hyperliquid/client.rs:745
// 无凭证 = 只接公共行情，没有账户可查，空仓是事实而非缺数据
```

同一事实存了两处：client 返回值的语义，与 `ExchangeSetup.authed: bool`。

### 目标形态

```rust
/// 公共数据：无凭证也能用
pub trait ExchangeClient: Send + Sync {
    fn exchange(&self) -> Exchange;
    async fn fetch_all_symbol_metas(&self) -> Result<Vec<SymbolMeta>, ExchangeError>;
    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError>;
}

/// 账户私有：只有配了凭证的所才有这个对象
pub trait AccountClient: Send + Sync {
    fn exchange(&self) -> Exchange;
    async fn place_order(&self, order: ExchangeOrder) -> Result<OrderId, ExchangeError>;
    async fn cancel_order(&self, symbol: &Symbol, order_id: &OrderId) -> Result<(), ExchangeError>;
    async fn fetch_pending_orders(&self, symbol: &Symbol) -> Result<Vec<OrderUpdate>, ExchangeError>;
    async fn fetch_account_info(&self) -> Result<AccountInfo, ExchangeError>;
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExchangeError>;
}

pub struct ExchangeSetup {
    exchange: Exchange,
    client: Arc<dyn ExchangeClient>,
    /// `None` = 只接公共行情。**`authed: bool` 随之删除** —— 有没有账户由类型说了算
    account: Option<Arc<dyn AccountClient>>,
    supports: fn(&SubscriptionKind) -> bool,
    spawn_actor: Box<dyn FnOnce(SpawnCtx) -> SpawnFuture + Send>,
}
```

各所的 client struct 仍可同时 impl 两个 trait（不必拆结构体）；**约束点在 `setup_*`**：
只有 `access.has_credentials()` 为真时才把它装进 `account: Some(..)`。这样"无凭证却存在
AccountClient 对象"在装配处被堵死，下游拿到 `Option` 即是权威。

### 改动清单

- `exchange/client.rs`：拆 trait；`ExchangeAccess::has_credentials` 的唯一消费者变成 `setup_*`
- `exchange/{binance,okx,hyperliquid,ibkr}/client.rs`：删掉全部"无凭证返回空"的降级分支，
  改为这些方法只存在于 `AccountClient` 上
- `engine/live/assembly.rs`：`authed: bool` → `account: Option<...>`
- `manager/mod.rs`：`clients` 拆成 `clients`（metas 用）与 `accounts`（下单/对账用）；
  `authed_exchanges: HashSet<Exchange>` 删除
- `OrderGateway` / `PositionLedgerActor`：持 `HashMap<Exchange, Arc<dyn AccountClient>>`
- `provisioning.rs`：启动撤残单、投产 REST 快照按 `accounts` 遍历，`if authed` 判断消失

### 验收

- `grep -rn "authed" src` 无结果
- `grep -rn "无凭证" src/exchange/*/client.rs` 无结果
- `PositionLedgerActor` 的 `clients.is_empty()` 早退分支语义不变（没有账户的部署仍能只跑行情）
- 现有测试全绿，`false-zero-audit.md` 附录 A 第 5 条（"无凭证 fetch_positions 返回空 Vec"）
  可以从"已核实的合法 0"里删除——它不再存在

### 风险

低。改动面大但机械，编译器会指出每一处。IBKR 无需判断：`IbkrClient::new` 本就要打网关鉴权，
只在有凭证时才构建，故 `account: Some(..)` 恒成立。

### 完成记录

332 用例通过，零编译告警，clippy 净增 0（改动前后同为 19 条既有告警）。

三处计划外但顺手的收获：

1. **`ManagerActor.clients` 字段变成死代码被删**。拆分后它的唯一用途（metas 预加载）只发生在
   `on_start` 的局部变量上，私有调用全部改走 `accounts` —— 编译器直接报 never read。
2. **`BinanceActorArgs.client` 字段删除**。它只为 equity 轮询而存在，而那是账户私有能力；
   actor 内部本就另建了一个带凭证的 `Arc<BinanceClient>`，改用它之后这个字段成了重复通道。
3. **三个测试替身瘦身**。`FakeClient` / `FaultyClient` 不必再桩掉 `fetch_all_symbol_metas`
   与 `fetch_symbol_meta` 两个它们永远不碰的方法 —— 接口收窄的收益在测试里立刻兑现。

### 审查发现的 Critical（已修）

拆分暴露了一个**既有的判据分叉**：投产的撤残单遇到"无账户所"跳过，而撤下的清理遇到同样
情形返回 `Err`。同一事实两套结论。无凭证配置（`adaptive_trade` 明文支持"只跑模拟"）下一旦
降级：executor 已停并移出登记，而 supervisor 收到 `Err` 会保持 `live = Some(..)` ——
记账与事实背离，该 symbol 此后晋升被拒、降级重试永远撞同一个 `Err`，**永久卡死**。

审查建议的首选修法是"投产处拒绝 Live + 无账户所"，**未采纳**：`exchange_symbols` 来自
`public_streams()`，是策略**读**的所而非**交易**的所。按它拒绝会误伤合法配置 ——
从 Binance 读盘口、只在 HL 下单。

实际修法是把判据收敛成一个函数 `ManagerActor::exchange_side_cleanup`，两条路径共用：
无账户 ⇒ 交易所侧不可能有本引擎的残留（下不了单、收不到私有流）⇒ 走 `finish_removal`
干净收尾（仍如实上报此前累积的 `incomplete`）。

**测试覆盖的是判据，不是完整撤下路径** —— `ManagerActor::on_start` 要联网建 client、
拉 symbol metas，单测里起不来（同 `stop_semantics_tests` 的既有取舍）。

顺带对齐的遗留：OKX 适配层的 `client: Option<_>` 现在也直接源于凭证（此前无条件传 `Some`
再用元组 bool 门控），与 Binance 同一模式。

---

## R2 拆 `StateManager`

### 现状

39 个公开方法的共享大对象，三组互不重叠的消费者。详见 architecture.md §4 V1。

### 目标形态

四个单一职责投影，各自按 symbol 索引：

| 投影 | 内容 | 消费者 |
|---|---|---|
| `MarketView` | bbos / mark_prices / index_prices / funding_rates / market_statuses | 策略、metrics |
| `PositionBook` | positions + position_seeded_at；`seed` / `apply_fill` / `seeded_positions` | 账本 actor、策略、metrics |
| `OrderBook` | pending_orders + recent_terminal + 超时清理 | 策略、metrics |
| `AccountView` | balances / account_infos / greeks / cash_balances | metrics（策略侧待核实，见下） |

消费者收缩：

- `PositionLedgerActor` 只持 `PositionBook`——它现在持的整个 `StateManager` 里有 30+ 个方法
  它一辈子不会调
- `StrategyRunner` 持三者的组合，并据此构造 R3 的策略视图
- `MetricsActor` 四个都要（它确实是全景观测），但它那份 `PositionBook` 复制**不会消失**

### 如实说明：这一步不消灭第三份持仓副本

metrics 仍需本地持仓，理由未变（`on_stop` 最终快照不能依赖并发停机的 actor）。R2 的收益不是
"三份变两份"，而是**那份复制从"拖着 39 个方法的大对象"变成一个几十行的小结构，且与账本
复用同一份 seed 语义与防双计规则**。architecture.md §2 P1 要求的"注明为什么不能消除"仍然适用。

不要在计划里把它写成"消灭了重复"——那是 1.5 里说的那种"竞态已消除"式虚报。

### 改动清单（分两步，中间保持可发布）

**R2a 纯搬运**：把字段分组为投影，公开方法全部委托。行为零变化。

**已勘定的分解**（动手前的实测，避免下次重新摸底）：

要拆的字段**大部分在 `SymbolState` 里，不在 `StateManager` 里**，所以刀口要切进 `SymbolState`：

```rust
// per symbol，三个投影
pub struct SymbolMarket   { funding_rates, bbos, mark_prices, index_prices }
pub struct SymbolPositions{ positions, seeded_at }
pub struct SymbolOrders   { pending_orders, recent_terminal }

// 门面保留 —— "一个 symbol 跨所的全貌"确实是策略推理的单位
pub struct SymbolState { symbol, market, positions, orders }

// StateManager 自己的字段归 AccountView
pub struct AccountView { balances, account_infos, greeks, cash_balances }
```

方法归属：

| 投影 | 方法 |
|---|---|
| `SymbolMarket` | `bbo` / `mark_price` / `index_price` / `unified_time_base` / `best_short_exchange` / `best_long_exchange` / 行情侧 `apply` |
| `SymbolPositions` | `seed_position` / `is_position_seeded` / `seeded_positions` / `has_positions` / `position` / `position_size` / `position_sizes` / Fill 累加（含 `snapshot_req_ts` 过滤） |
| `SymbolOrders` | `add_pending_order` / `remove_timed_out_orders` / `has_pending_orders` / `has_pending_side` / `pending_orders` / `remember_terminal` / OrderUpdate 侧 `apply` |

**两处要注意**：

1. 几个方法的日志里读 `self.symbol`，而投影不该持有 symbol（那会让三份各存一个副本）。
   改为把 `&Symbol` 作为参数传入 —— 只有 `seed_position` 与 `remove_timed_out_orders`
   两个方法需要。
2. `SymbolState` 的 `pub` 字段外部只有 **2 处**直接访问（实测）：
   `executor.rs` 的 `symbol_state.positions.values()`、`spread_arb` 测试里的
   `state.bbos.insert(..)`。所以"零消费者改动"做不到，但改动面就这么大 ——
   后者更应该改成走 `apply` 喂事件，与生产路径一致。

`SymbolState::apply` 的拆法：按 variant 分派给对应投影，**保留"未知 symbol 事件 = 路由 bug"
的 error 日志**（现在这条判断在门面上，拆完仍应在门面上，不要让三个投影各自静默忽略）。

**R2b 切换消费者**：逐个消费者改为只持它需要的子结构。

### R2b 完成记录

**持仓账本从 39 个方法收到 8 个。** 新增 `PositionBook`（跨 symbol 的 `SymbolPositions`
容器），`Reconciler.mirror` 的类型从 `StateManager` 改为它 —— 类型就是职责声明，
编译器保证账本碰不到行情缓存、挂单跟踪、账户净值、希腊值。

**防双计规则单一化**（这一步的正确性前提）：「seed 之后、快照请求时刻之前送达的 Fill
要丢弃」原先写在 `SymbolState::apply_account_data` 的 Fill 分支里。账本独立之后若不动它，
这条规则就会有两份 —— 而两份的差异表现为"对账偶尔误报漂移"，是最难查的一类。
故提取为 `SymbolPositions::apply_fill`，与 `seeded_at` 同住一个结构，门面与账本共用。

**其余两个消费者不动，理由已核实**：

- `StrategyRunner` 需要全部四个投影（策略实测用 11 个方法，跨四个投影）
- `MetricsActor` 同样需要四个：`account_infos()` 走账户投影、`exposure()` 要
  持仓 × 价格的 join、`pending_orders()` 要挂单。它是全景观测，本就该看全。

所以"持仓折叠三份宿主"的数量没变，但**性质变了**：账本那份现在是最瘦、最专一的，
而它正是唯一被 REST 持续校验的那份。这与 architecture.md §4 V1 的记录一致。

### R2a 完成记录

四个投影已落地并导出：`SymbolMarket` / `SymbolPositions` / `SymbolOrders`（per symbol，
由 `SymbolState` 门面组合）+ `AccountView`（账户级，挂在 `StateManager` 上）。
既有调用面全部保留为委托方法，故消费者改动极小。335 用例通过，clippy 净增 0。

三处实现决定：

1. 两个方法（`seed` / `remove_timed_out`）的日志要 symbol，改为**传参**，而不是让三个投影
   各存一份 symbol 副本。
2. `SymbolPositions::all()`（全部腿，含未 seed）与 `seeded_positions()`（仅已 seed）并存，
   语义分明：前者供降级平仓取量（"本实例账上有什么"），后者供对账与对外快照
   （"哪些可参与比对" —— 未 seed 是未知，不是空仓）。
3. `apply` 的路由 bug 判断（"未知 symbol 事件"）留在**门面**上，不下放给三个投影 ——
   否则每个投影都要各判一次，判漏了会静默忽略。

`market_statuses` 留在 `StateManager` 上，没有并进 `AccountView`：它是公共行情
（不属于账户），又是 per-exchange 而非 per-symbol（进不了 `SymbolMarket`）。

**测试夹具改走生产路径**：`state.rs` 与 `spread_arb` 的夹具此前直接写 `pub` 字段
（`state.bbos.insert` / `state.positions.insert`）。字段私有化后改为盘口走 `apply(行情事件)`、
仓位走 `seed_position`。直接写字段更省事，但那样测的就不是线上跑的那条路。

### 一处影响 R3 的更正

统计策略依赖面时我漏了账户级方法（当时只扫了变量名 `state` 的调用点，而策略里那三处叫
`state_manager`）。实际是 **11 个**而非 8 个：per-symbol 八个之外，还有 `equity` /
`account_info`（`spread_arb` 的杠杆闸门）与 `greeks`（`gamma_scalp` 的 delta）。

**R3 的策略视图必须包含 `AccountView`**，不能只给 per-symbol 那三份。

### 验收

- `PositionLedgerActor` 的字段类型是 `PositionBook`，不是 `StateManager`
- `SymbolState::exposure` 不再存在于领域层（并入 R5 的 V7 处理）
- 每个投影的公开方法数与其消费者的实际调用面接近（差距 > 3 倍要说明理由）
- 回测与实盘仍共享同一份 `StrategyRunner`（品味 1.6 不能因这次拆分破掉）

### 风险

**本轮最高**。触及 executor / 账本 / metrics / 四个策略实现 / 回测引擎。缓解：

- R2a / R2b 分开提交，R2a 之后可以停下来发布
- 拆分过程中 seed 语义与防双计规则（`seed_position` 的 `snapshot_req_ts` 过滤）必须原样搬过去，
  它有 5 条回归测试守着，任何一条红就是搬错了
- `greeks()` / `usdt_balance()` 等方法**先核实真实调用方**：四个策略实现的 grep 显示无人读账户类
  数据，若确认只有 metrics 用，`AccountView` 就不必进策略视图。**核实前不要假设它是死代码**

---

## R3 策略只拿受限视图

依赖 R2。

### 目标形态

```rust
fn on_event(&mut self, event: &IncomeEvent, view: &StrategyView<'_>) -> Vec<OutcomeEvent>;
```

`StrategyView` 构造时按**本实例的订阅范围**裁剪，范围外的 symbol 拿不到 `SymbolView`：

```rust
impl StrategyView<'_> {
    /// 范围外返回 None —— 不返回默认值（品味 1.3）
    pub fn symbol(&self, symbol: &Symbol) -> Option<SymbolView<'_>>;
}

impl SymbolView<'_> {
    pub fn bbo(&self, exchange: Exchange) -> Option<&BBO>;
    pub fn position_size(&self, exchange: Exchange) -> f64;
    pub fn position_sizes(&self) -> (f64, f64);
    pub fn has_pending_orders(&self) -> bool;
    pub fn pending_orders(&self) -> impl Iterator<Item = &PendingOrder>;
}
```

方法集就是实测的 8 个调用面，不多给。

### 一个必须先定的设计点

**裁剪不能用"返回默认值"实现**，否则正好违反品味 1.3。所以范围外走 `Option`，
沿用现有 `state.symbol_state(sym) -> Option<_>` 的形状，策略代码改动很小。

但 `position_size` 保持返回 `f64` 有一个待核实的口子：**Live 账户 + 无凭证的所**，那条腿
未 seed，本地持仓是"未知"而 `position_size` 会报 0。R1 之后这个组合是否还可达要核实；
若可达，`SymbolView` 必须让未 seed 的所不可查，而不是报 0。

### R3 完成记录

**策略契约从 39 个方法收到 9 个**：`StrategyView`（`symbol` / `equity` / `account_info` /
`greeks`）+ `SymbolView`（`bbo` / `position_size` / `position_sizes` /
`has_pending_orders` / `pending_orders`）。`Strategy::on_event` 的第二个参数按值传入
`StrategyView<'_>`（两个视图都是 `Copy` 的引用包装，无分配、无 dyn 分发）。

**越界投递堵在分发层，不在视图上。** 新增 `IncomeEvent::delivery() -> Delivery`
（`Symbol(所, symbol)` / `Exchange(所)` / `Broadcast` 三档，取代原来的
`routing() -> Option<(Exchange, Symbol)>`）与 `SubscriptionScope::accepts`。
实盘 `IncomeProcessorActor` 与回测循环调同一份判据；`by_symbol` 反向索引降级为纯粹的
热路径扇出优化，键由 `SubscriptionScope::pairs()` 灌，与判据同源。

中间那档是这次真正堵住的东西：`Balance` / `AccountInfo` / `Greeks` /
`ExchangeStatus` / `ExchangeRate` 没有 symbol，此前一律广播。

> **第一版走错了方向，记在这里。** 我先把裁剪做在 `StrategyView` 上（查询时按订阅所
> 过滤），并在 architecture.md 里把 V2 整条标成"已修复"。审查指出：那既是第三处
> "什么算范围内"的实现（P1/P4），又**根本挡不住** —— 越界的读数照样随 `&IncomeEvent`
> 推给策略，payload 里就带着净值，只堵查询不堵投递等于没堵。
>
> 教训与品味 1.5 是同一条：**堵了一半就声称堵住，比不堵更糟**——后来的人会以为这条
> 已经关死。判断一个"防线"是否成立，要问的是**数据还有没有别的路到达**，
> 而不是"我拦的这条路拦住了吗"。

**顺手删掉的死接口**：`IncomeEvent::routing()`（被 `delivery()` 取代）、
`StateManager` 上九个账户转发方法（`equity` / `account_info` /
`greeks` / `usdt_balance` / `total_usdt_balance` / `total_equity` / `account_notional` /
`total_account_notional` / `account_infos`）、`has_pending_orders`、`seeded_positions`。
前九个是 R2 为"保持既有调用面不变"留的过渡层，策略改走视图后同一个事实有两个入口
（P1）；后两个在消费者迁移后彻底无人调用。账户读数现在只剩 `account_view()` 一条路。

### 那个"待核实的口子"：核实结果是不必改

`SymbolView::position_size` 保持返回 `f64`。投产握手对**每个有 `AccountClient` 的所**
都拉基线，且拉取失败直接拒绝启动（`provisioning.rs` 的 `fetch_baselines`）——
所以实盘可下单的所必然已 seed。没有 `AccountClient` 的所确实不 seed，但引擎在那里
从来不会也不能下单，"本地持仓 0" 是事实而非缺数据。

改成 `Option<f64>` 会让全部 14 个调用点处理一个它们无法据以行动的情况
（"这个所不可交易"应当在投产期一次性拒绝，不是每次读持仓时判一遍）。

### 测试夹具：卡点的解法

上次卡在 `spread_arb` 的两个独立夹具（一个 `SymbolState`、一个 `StateManager`）。
合并成一个 `Fixture`（持底层 `StateManager`，`view()` / `symbol()` 按需借出），
`fixture(&[..]).with_account(equity, notional)` / `.mispriced()` 链式构造。

**顺带修掉一个测试本身的毛病**：旧形态下 `check_signal(&state, &manager, ..)` 的两个参数
可以描述互相矛盾的世界（`mispriced_state()` 造的错价盘口压根不在 `manager` 里），
而线上它们必然来自同一个 `StateManager`。合并夹具之后这种形态连编译都过不去。

基线快照时刻取 `SNAPSHOT_TS = 1` 而非 0：`local_ts == snapshot_req_ts` 的 Fill 会被
防双计规则丢弃，用 0 会让日后新增的 `local_ts = 0` 的 Fill 测试静默失效。

### 验收

- `Strategy::on_event` 签名不再出现 `StateManager`
- `StrategyView` 的公开方法数 ≤ 10
- 新增一个"策略读范围外 symbol"的测试，断言拿到 `None`
- 四个策略实现改动都在几十行量级（若某个实现要大改，说明视图裁剪错了）

### 风险

中。改动集中在 trait 与四个实现，但 `spread_arb` 有 1648 行，需要逐个调用点核对。

### 一次未完成的尝试留下的结论（附一处自己犯的错）

**回滚没滚干净**：`git checkout -- .` 只回滚**已跟踪**的修改文件，新建的
`src/strategy/view.rs` 是未跟踪文件、没被回滚，随后 `git add -A` 把它扫进了那个
docs commit —— 于是仓库里躺了一个 109 行、没在 `mod.rs` 里声明、**不参与编译**的孤儿文件，
而 commit message 明写着"代码已回滚，未留半成品"。

这正是品味 1.5 说的"比不写更糟"的那类声明。教训写在这里：
**声称回滚之后要 `git status` 确认，未跟踪文件不在 `checkout --` 的射程内。**


生产代码部分改完是顺的（trait 签名、`StrategyRunner` 构造视图、`gamma_scalp`、
`spread_arb`、三个 bin 策略），**卡点在 `spread_arb` 的测试夹具**：

约 30 处调用点把 `symbol_state()`（返回 `SymbolState`）与 `view()`（返回 `StateManager`）
当**两个独立夹具**用。改成视图之后 `SymbolView` 与 `StrategyView` 都要从**同一个**
底层 `StateManager` 借出，两个夹具必须合并成一个，30 处调用点随之全改。

下次做时先做这一步：引入一个持有底层 `StateManager` 的测试夹具类型，按需借出两种视图，
再改生产代码。反过来做会卡在中间。

### 另一处要在文档里更正的话

V2 初稿的理由之一是"策略能看到其他 symbol 的状态"——**这是错的**。`StrategyRunner`
用策略自己 `public_streams()` 推导出的 symbol 集合去建 `StateManager`，所以它能查到的
symbol 本来就只有自己订的那些。

R3 真正的收益是两条，比初稿写的窄：

1. **接口最小化**：39 个方法收到 11 个，且往引擎状态里加字段不会自动扩大策略可见面
2. **账户数据按订阅范围裁剪**：`equity` / `account_info` / `greeks` 按交易所索引，
   此前不过滤 —— 策略可以读到它压根没订阅的交易所的净值。这是**确实存在**的越界

---

## R4 下单出口的分发判据收敛到一处

### 现状

```rust
// outcome_processor.rs:447
if tagged.account != AccountId::Live { return; }
// paper_counter.rs:348
if !tagged.account.is_paper() { return; }
```

### 目标形态

单一穷举判据 + 装配处过滤，消费者 handler 里不再自判：

```rust
/// 一条策略信号该由哪个出口执行。**唯一判据**，两处订阅过滤都调它。
/// 新增 `AccountId` variant 时此 match 不穷举 -> 编译失败（P6）。
pub enum Outlet { Live, Paper }

pub const fn outlet_for(account: &AccountId) -> Outlet {
    match account {
        AccountId::Live => Outlet::Live,
        AccountId::Paper(_) => Outlet::Paper,
    }
}
```

manager 装配处：

```rust
outcome_pubsub.tell(SubscribeFilter(gateway_consumer, |o| matches!(outlet_for(&o.account), Outlet::Live)));
outcome_pubsub.tell(SubscribeFilter(paper_counter,    |o| matches!(outlet_for(&o.account), Outlet::Paper)));
```

两个 filter 挨着写，新增出口时在同一屏可见；判据本身只有一份。

### 验收

- 两个 handler 里的 `if ... return` 早退删除
- 新增一条测试：对每个 `AccountId` 构造样本，断言**恰有一个** `Outlet`（不重不漏）
- 人为增加一个 `AccountId` variant 时 `outlet_for` 编译失败（改动时手工验证一次，记录在 commit）

### 风险

低。但要注意：删掉 handler 里的早退等于把正确性完全押在订阅过滤上，
**必须确认没有第二条路径能把 `AccountOutcome` 送进这两个 actor**（目前只有 OutcomePubSub 一条）。

### 完成记录

335 用例通过，零编译告警，clippy 净增 0。

- `Outlet` + `outlet_for` 落在 `engine/live/mod.rs`（`AccountOutcome` 旁边），
  订阅过滤器提成具名函数 `to_live_outlet` / `to_paper_outlet` —— 这样测试断言的是
  **装配处真正用的那两个函数**，而不是测试里复写一遍的等价表达式。
- 投递路径唯一性已核实：`AccountOutcome` 的生产路径只有 `ExecutorActor` 经
  `OutcomePubSub` 一条；`paper_counter.rs` 里三处直发都在它自己的测试模块内。
- 穷举保护已实测：临时给 `AccountId` 加一个 variant，`outlet_for` 与
  `MetricsActor` 的账户分支都编译失败。

**与计划的两处偏离**：

1. **保留了到站断言**，没有裸删早退。删掉自判之后投递路径在类型上收不死
   （`SubscribeFilter` 是运行期的），装配写错就会静默双执行。所以两个出口各留一句
   `if outlet_for(..) != 本出口 { error!; return }`。这**不是**重新分发 ——
   判据仍是同一个 `outlet_for`，新增账户类型时它编译失败，不存在"两处各判各的"
   静默漏改；这是品味 1.1 阶梯上的第二级（运行时断言），因为第一级够不着。
2. **顺带把 `MetricsActor` 的账户分支也改成穷举**（原为 `if account != Live`）。
   它不是出口分发，但同样会在新增账户类型时把新类型静默并进模拟账本。
   **没有**复用 `outlet_for` —— "订单发往哪个执行出口"与"记进哪本账"是两件事，
   恰好同形不等于同一个概念，合并会是假抽象。

---

## R5 收尾

### V6 投产编排独立

`provisioning.rs`（1039 行）是独立领域流程，与监督/停机没有内在联系。抽成一个持有所需句柄的
`Provisioner`，manager 只转发。**不新增 actor**——它仍在 manager 的邮箱里串行执行，
只是不再和监督/停机的代码混住。

收益中等，主要是让 manager 从"四件事"回到"两件事"（监督树根 + 停机链根）。

### V7 观测口径移出领域层

`SymbolExposure::session_notional_delta` 与 `SymbolState::exposure` 移到 `MetricsActor`
（唯一生产调用方，`metrics.rs:160`），改为接收 `PositionBook` + `MarketView` 的自由函数。
R2 完成后这一步几乎是顺手的。

---

## 有意不做

- **V5 停机三段模型的回灌边**。已论证救不回来（柜台延迟管道是 spawn 的，`on_stop` 不排空），
  代价是最终一次指标快照少记一笔，且这批数据一个字节都不持久化。见 `manager/mod.rs:107-119`。
- **把 `Exchange` 从枚举换成开放类型**（字符串 / trait object）。枚举的封闭性在这里是**资产**：
  P6"漏改必须编译失败"就靠它（`ALL_EXCHANGES` 常量存在的唯一理由）。加新所需要动枚举，
  这个成本是有意保留的。
- **消灭 metrics 的持仓副本**。理由见 R2"如实说明"一节。
- **给快照面加通用查询消息** (`Query { kind }`)。假抽象，见 external-data-access.md 契约 4。
- **在账本的拉取上再叠一层超时**。各所 client 自己配了超时，在途停摆守卫已兜住后果，
  再加一层是把同一职责实现两遍。

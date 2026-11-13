# 引擎对外开放数据的统一机制

外部代码（观测导出、通知、复盘账本、运维脚本）需要引擎里的数据。本文定义**两个面**及各自契约：
数据按性质选面，不为单个消费者开特例。

```text
                      ┌────────────────────────────────────────┐
   流（推）            │  market_bus() / account_bus()          │
   连续、增量、有序     │  outcome_bus()  + inbox.subscribe      │
                      └────────────────────────────────────────┘
                      ┌────────────────────────────────────────┐
   快照（拉）          │  all_symbol_metas()                    │
   当下值、可采样       │  live_positions()                      │
                      └────────────────────────────────────────┘
```

两个面都经 `ManagerHandle`（`run_engine` 交给装配闭包的句柄）。

## 一、流面：三条总线

三条总线覆盖引擎里全部事件类型，订阅时按 `fn` 指针过滤：

| 总线 | 覆盖 |
|---|---|
| `market_bus()` | `MarketData` 全部 variant：BBO、MarketTrade、MarkPrice、IndexPrice、FundingRate、Candle、HistoryCandles、BorrowFee、ExchangeRate、ExchangeStatus、Clock、Custom |
| `account_bus()` | `AccountData` 全部 variant：OrderUpdate、Fill、Balance、FundingFee、Greeks、AccountInfo |
| `outcome_bus()` | 策略信号（下单 / 撤单 / 自定义事件），带 `account` 标签 |

**契约**

- 订阅只能经收件箱（`Inbox::subscribe`），收件箱只能从任务树上的 `Scope` 建 —— 观察者经
  `ManagerHandle::observers()` 挂到 manager 的消费者组，在自己的构造闭包里订阅。"订一个没人看着的组件"
  不可表达：观测组件悄悄死掉而引擎照常交易，是最危险的一类故障（挂在树上的观察者出错即整机停机）。
- 过滤器是 `fn` 指针而非闭包：不许捕获状态，因此筛选判据是纯粹的事件形状判断，可被独立推理。
- **必须带 `account` 判据**：实盘与模拟账户共用 `AccountBus`，靠事件自带的 `account` 区分。不筛的话
  模拟成交会混进实盘口径。
- 投递进订阅者的无界收件箱：消费者跟不上时后果是积压（可见，见 `Executor::check_pipeline_lag`），不是丢事件。

**新增一个事件类型时，流面不需要任何改动** —— 订阅方的 filter 自动可见新 variant，manager 一行不用改。

## 二、快照面

有些数据不是流。目前两项（`ManagerHandle` 的方法）：

| 方法 | 应答 | 语义 |
|---|---|---|
| `all_symbol_metas()`（同步） | `HashMap<Exchange, Vec<SymbolMeta>>` | 各所交易规则（精度、步长、合约乘数）；读句柄里那份启动时预加载、此后不变的目录 |
| `live_positions().await` | `Result<Vec<Position>, String>` | 实盘持仓真值，**币本位**，per (所, symbol)；直接问持仓账本 |

**契约**（每一项都必须满足，否则不该进这个面）

1. **纯内存读**。应答路径上不许有任何 IO —— 不打 REST、不读文件、不推送。
2. **不阻塞收件箱**。**数据宿主**（真正持有状态的那个组件）处理**任何**消息时都不得 `await` 在
   IO 上，否则查询会排在一次网络超时后面。这条约束反向淘汰了宿主候选：
   见下文"为什么持仓查询挂在账本上"。
3. **只经 `ManagerHandle` 暴露**，外部只持有这个句柄。内部组件的地址不外泄 —— 否则外部能给它发
   任何消息。句柄的快照方法不经 manager 的收件箱：品种目录直接读句柄里那份，持仓直接问账本。
4. **不许合并成一条通用查询**。`query(kind: Positions | SymbolMetas) -> QueryResult` 是"流程强行合并后
   用 `match` 分发"的假抽象。新增一类快照 = 新增一个句柄方法，不改既有分支。

（旧版快照由 manager 的邮箱应答，投产期会排队秒级；#704 起不经 manager，这条代价已不存在。）

## 三、持仓：唯一需要专门安排的一项

### 为什么它不能只靠流面

持仓是**对 Fill 流的折叠**，而折叠需要初值。其余"当下状态"都不需要专门安排：

| 数据 | 外部怎么拿 | 为什么不需要初值 |
|---|---|---|
| 净值 / 余额 / 交易所状态 | 订 `AccountData::AccountInfo` 等 | 被周期性轮询并**重发**，一个周期内自然拿到最新值 |
| 当前挂单 | 折叠 `OrderUpdate` 流 | 初值是**已知常量**：引擎启动先撤全部残单，起点必为空 |
| 累计成交 / 手续费 / 资费 | 折叠 `Fill` / `FundingFee` 流 | 口径是"本次会话起"，起点就是 0 |
| **持仓** | **查 `live_positions()`** | 初值 = 交易所侧既有存量，非常量；且**刻意不周期性重发** |

最后一条是设计决定：持仓真值由「投产期 REST 基线 + 之后全程 Fill 累加」维护，REST 只用于对账，
**不许覆盖**权威状态。所以外部无法靠"等下一次快照"拿到起点。

### 外部消费者不该自己维护持仓投影

复刻「基线 + Fill 累加」是可行的，但代价是一份会漂的副本：它不受对账保护，漂了没人知道。
引擎里那一份每 3 秒被 REST 全量校验，差值稳定超容差即致命退出。

**所以外部要持仓就查，不要自己算。** 采样语义对 gauge 完全够用。

## 四、持仓真值的三份宿主：一份权威，两份有理由的本地派生

如实记录：这个折叠在进程里有三份，不是一份。收不成一份，理由必须写下来，否则下一个人会再加第四份。

| 宿主 | 角色 | 为什么不能消除 |
|---|---|---|
| `PositionLedger.reconciler.mirror` | **权威 + 对外** | 它是唯一被 REST 持续校验的那份，因此唯一有资格叫真值 |
| `Executor` 的 `StrategyRunner.state` | 决策用 | 策略是**纯函数**，决策中途不能 `await` 去查询 |
| `Metrics` 的本地状态 | 报告用 | 需要 持仓 × 价格 的 join；且收尾时的最终快照不能依赖一个**正在并发停机**的组件（`consumers` 组内并发停机） |

三份都是同一条 Fill 流、同一份 seed 语义（`SymbolState::seed_position`）、同一条防双计规则的确定性结果，
因此除非有 bug 不会分叉。**新增第四份必须先证明它不能用 `live_positions()` 替代。**

「除非有 bug」不靠信：executor 那份是下单依据，它与账本的分歧由账本每个节拍检查（#694）——
账本向每个实盘 executor 要一份已 seed 的持仓（`ExecutorHandle::seeded_positions`），与镜像按同一判据比对
（差值稳定连续 N 次即致命）。于是 executor ≈ 账本 ≈ 交易所两条比对串起来。metrics 那份只用于报告，
不纳入。

### 为什么持仓查询挂在账本上

契约 2（宿主处理任何消息都不得阻塞在 IO 上）淘汰了其他候选：

- `Metrics` —— 推送交给它的子任务推送器（`observability::prometheus::Pusher`），收件箱不碰 IO，**合格**；但它是观测组件，
  明确声明"故障不得拖垮交易"，让它当权威数据源会反转这条边界。
- `PositionLedger`（原 `PositionReconcileActor`）—— 原本**不合格**：`poll_and_reconcile` 在 handler 里
  `join_all` 各所 REST，最长阻塞一个 client 超时。这次把拉取移出 handler（见下）后合格，且它本就是
  被校验的那一份 —— 外部读到的数字与风控守卫看到的数字**是同一个**。

## 五、本次改动清单

> 历史记录（kameo 版当时的改动），名字保留原样：`PositionLedgerActor` → 现 `PositionLedger`，
> `GetLivePositions` → 现 `ManagerHandle::live_positions`，`ExecutorActor::GetPositions / GetSeededPositions`
> → 现 `ExecutorHandle::positions / seeded_positions`。

### A. 账本 actor 的邮箱不再阻塞在 REST 上（前置缺陷修复）

原实现在节拍 handler 里 `await` 各所 REST，副作用不止"查询排队"：它自己的文档已承认会把
**本轮比对与 Fill 摄入一起推迟**至多一个超时周期。改为：

- 节拍到达 → `tokio::spawn` 各所拉取 → 结果回投为 `PositionsFetched { exchange, result }` 消息
- `in_flight: HashSet<Exchange>` 防重入：某所上一轮未回则本轮跳过并 warn（否则慢 REST 会堆积并发请求）
- 连续不一致判据与致命退出语义不变

**停摆守卫必须补两道，否则这次改动会架空它**（审查发现的 Critical，已修）：

`StalenessGuard` 原本只有两个触点 —— `record_success` 与 `check_failure`，**都在结果回投的路径上**。
把拉取移出 handler 之后，"结果永不回投"变成可达状态：拉取任务 panic（被 `tokio::spawn` 吞掉）、
或 client 超时失效导致请求真挂死。那时 `in_flight` 永久占位、每个节拍只是跳过，守卫永远不被评估 ——
**该所对账静默永久失效，而引擎继续无校验交易**。这比漏报单次漂移严重得多，因为它没有任何外在症状；
相对旧实现还是个回归（旧代码里 panic 发生在 handler 内，actor 以 `Panicked` 终止、级联整机退出，是可见的）。

两道各管一段：

1. **在途即过守卫**（`check_in_flight_staleness`）：节拍发现某所仍在途时，对它调用
   `check_failure` —— "在途未回"与"拉取失败"同等对待，都是没拿到读数。判据仍是"距上次成功的时长"。
   这一道兜住"连回投本身都没发生"的一切成因。
2. **panic 转 Err 回投**：拉取再套一层 `tokio::spawn`，`JoinError`（panic 或取消）被翻译成
   `Err` 回投。adapter 里的 `unwrap` 本该是**立即可诊断**的故障，不该退化成 60 秒后一句"通道停摆"。

**不再加超时层**：各所 client 自己配了超时，请求挂死属于 client 的缺陷，第 1 道已经兜住后果。
在这里再叠一个超时是把同一个职责实现两遍。

**行为变化（如实声明）**：此前 `join_all` 期间 Fill 排队不入镜像，比对用的镜像时点**早于**快照，
偏差方向是"本地滞后交易所"；改后 Fill 连续摄入，镜像时点**晚于**快照，偏差方向反转为"本地领先交易所"。
两者都是有界的在途窗口、差值每轮都变，由"差值稳定才计数"的判据兜住；新行为更如实（镜像不再被自己的 IO 冻住）。

### B. 更名 + 开放查询

- `position_reconcile.rs` → `position_ledger.rs`；`PositionReconcileActor` → `PositionLedgerActor`；
  `PositionReconcileArgs` → `PositionLedgerArgs`。纯逻辑核心 `Reconciler` 保留原名（它的职责仍是比对）。
- 新增 `GetLivePositions`，应答 `Vec<Position>` = **全部已 seed 的腿**（含 `size == 0` 的空仓腿）。
  未 seed 的腿**不返回** —— 那时本地持仓是"未知"而不是 0，把未知当空仓报出去会让下游算出假的对冲完整性。

与 `ExecutorActor::GetPositions` 的区别（两者都保留，语义不同）：

| | 视角 | 含模拟账户 | 用途 |
|---|---|---|---|
| `GetLivePositions` | 实盘权威真值 | 否 | 对外快照、观测 |
| `ExecutorActor::GetPositions` | 某策略实例内部状态 | 是 | 降级平仓取量（须在 kill 该 executor 之前发起） |
| `ExecutorActor::GetSeededPositions` | 某策略实例的已 seed 腿 | 只问实盘实例 | 持仓账本拿它与镜像比对（#694）；未 seed 的腿是「未知」，不给 |

### C. manager 的快照面

`GetLivePositions` 挂到 `ManagerActor` 并转发给账本 actor，与 `GetAllSymbolMetas` 同形。

## 六、下游改造示例

以"三个 gauge：`position` / `position_notional` / `net_exposure`"为例。

改造前：自建 `position_state: HashMap<(Exchange, Symbol), f64>`，靠总线上的持仓基线定起点 + Fill 累加。
基线退出事件流后这条路断了。

改造后：**删掉 `position_state` 与 `baselined` 两个字段**，推送前查一次：

```rust
// 观察者里存一份 ManagerHandle（run_engine 交给装配闭包的那个，可克隆）。
// Err（账本不可达或引擎在停机）意味着**取不到**，绝不能当成"没有持仓"。
// 取不到时正确的动作是这一轮不更新 gauge（保留上一次的值），而不是把它们清零。
let positions = match self.manager.live_positions().await {
    Ok(positions) => positions,
    Err(reason) => {
        tracing::error!(%reason, "持仓快照取不到，本轮不更新持仓 gauge");
        return;
    }
};

for p in &positions {
    self.position_gauge
        .with_label_values(&[exchange_to_label(p.exchange), &p.symbol])
        .set(p.size);
    // 名义价值仍由本地 bbo_cache 提供价格：估值口径属于消费者，引擎不替它选 mid/mark/last
    if let Some(&(bid, ask, _)) = self.bbo_cache.get(&(p.exchange, p.symbol.clone())) {
        self.position_notional_gauge
            .with_label_values(&[exchange_to_label(p.exchange), &p.symbol])
            .set(p.size.abs() * (bid + ask) / 2.0);
    }
}
```

`net_exposure`（现货腿 + 永续腿）**留在下游算**：跨 symbol 的配对关系来自下游配置，引擎不知道，
也不该知道 —— 引擎侧 `hft_position_net_notional` 是"同一 symbol 跨所求和"，语义不同，不能替代。

**注意**：查询返回里没有的腿意味着"未 seed 或未跟踪"，即**未知**。这种腿应当**不上报 gauge**，
而不是上报 0 —— 后者会让"未知"在看板上伪装成"已平仓"。

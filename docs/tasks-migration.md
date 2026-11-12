# kameo → `crate::tasks` 迁移约定（#704）

`src/tasks`（tree / inbox / bus）是新的运行时，先读它三个文件的模块文档。本文件是迁移全部
actor 的统一约定，迁移中的每个会话都按它做；完成后本文件删掉，要点并入 architecture.md。

## 目标
- 删掉 `kameo`、`kameo_actors` 依赖，`src/actor_lifecycle.rs`，以及所有为绕 kameo 写的代码
  （`ChildGroup/ChildStop/ChildRegistrar`、`Supervised`、`spawn_supervised(_by)`、`RegisterSupervisedChild`、
  `on_link_died`、`dispatch_ws_stream_message!`、`impl_exchange_actor_ops!`、流结束 `kill()` 自己的约定、
  轮询器丢弃积压过时拍的逻辑）。
- 业务语义一律不变：下单、对账、持仓、指标、策略投产/撤下、告警、停机顺序都和现在一样。
  旧代码文档里写的每一条业务规则（为什么这样做）要跟着搬过去，只删 kameo 相关的解释。
- `cargo test`（lib + tests/ 下不联网的）、`cargo build --bins`、funding_arb 的 zigbuild 都通过。

## 组件写法（对照表）

| kameo | tasks |
|---|---|
| `impl Actor` + `on_start(args, actor_ref)` | `group.spawn(name, move \|scope\| async move { ...; Ok(State{..}) })`，构造逻辑写在闭包里；返回 `ChildHandle` |
| `on_stop` | `impl Task for State { async fn stop(&mut self) }`（子任务全部收尾完才调） |
| `impl Message<M>` + `tell/ask` | 每个组件一个 `enum Msg`，收件箱 `let (addr, inbox) = scope.inbox::<Msg>()`；对外给一个可克隆的 `XxxHandle(Addr<Msg>)`，带类型化方法；ask 用 `addr.ask(\|tx\| Msg::Get(tx)).await` |
| `ActorRef<X>` 字段 | `XxxHandle` 或 `Addr<XMsg>` |
| `attach_stream(stream)` | 流由任务自己持有，在 `run` 的 `select!` 里 poll |
| 定时流（`IntervalStream`） | `tokio::time::interval` 放在 `run` 里，`MissedTickBehavior::Skip`（不积压，自然取代"丢弃积压过时拍"） |
| 流结束 / 解析失败 → `ctx.actor_ref().kill()` | `run` 返回 `Err(..)`（带上下文） |
| `PubSub<E>` + `Publish` | `tasks::Bus<E>`，`bus.publish(e)`（同步、不会失败） |
| `Subscribe/SubscribeFilter` | 订阅者在自己构造时 `inbox.subscribe(&bus, filter_fn, Msg::Variant)` |
| `ChildGroup::spawn` / `push_handle` | 构造阶段 `let g = scope.group("名")` 声明分组，往里 `spawn`；**分组只能在构造阶段声明**，停机按声明顺序 |
| `ChildGroup::remove` + `stop_gracefully` 单个子 actor | `child_handle.stop().await` |
| `EngineHalt::kill(manager)` | 根的 `FailHandle`（`fail(anyhow!(..))`） |
| `on_link_died` 判据 | 框架内建：被要求停止后返回 Ok = 正常；Err/panic/未经请求返回 Ok = 出错上冒 |

### run 的标准形状
```rust
impl Task for Foo {
    async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(self.every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                msg = self.inbox.recv() => match msg {
                    Some(m) => self.handle(m).await?,
                    None => return Ok(()),          // 已被要求停止，箱已排空
                },
                _ = tick.tick() => self.poll().await?,
                frame = self.ws.recv() => self.on_frame(frame.context("WS 流结束")?)?,
            }
        }
    }
}
```
- 有收件箱的组件**用 `inbox.recv()` 返回 None 作为停止信号**，不要另写 `scope.stopping()` 分支直接 break
  （会丢箱里的消息，Inbox drop 时会报 ERROR）。没有收件箱的用 `_ = scope.stopping() => return Ok(())`。
- 处理一条消息出错要整支停：`?` 返回 Err。原来"记日志继续"的地方保持记日志继续。
- 收尾时要补发的东西（如 IBKR 等 commission 的成交）**直接发到总线**，不经父节点转（父节点收件箱先关）。

### 发送失败怎么处理
- `bus.publish` 不会失败。
- `addr.send` / `ask` 返回 `Gone` = 对方已停止收件。只可能发生在停机途中或对方已出错退出（出错会自己上冒）。
  所以：`scope.is_stopping()` 时记 warn 忽略；否则返回 Err（"不该发生"的地方要大声）。

## 拓扑（迁移后）

```
Root("engine")                         ← bootstrap::run_engine 持有，唯一 30s 总期限、二次信号直接退出
└─ group "main"
   └─ Manager（Task）
      ├─ group "producers"   交易所节点（各所 Actor + 其 WS/轮询子任务）、Clock、策略 executor
      └─ group "consumers"   IncomeProcessor、OutcomeProcessor、PaperCounter、Metrics、PositionLedger、外部观察者
```
- 三条总线（`MarketBus`/`AccountBus`/`OutcomeBus` = `tasks::Bus<..>`，别名替换 `MarketPubSub` 等）是
  普通值，由 manager 构造时建好、克隆给各组件；**不再有"总线一组"**，manager 停机两段：生产者 → 消费者。
  manager 文档里关于"三段/回灌边"的说明按新事实改写（回灌边：柜台/网关在消费者组内互发的在途回报，
  停机时收件箱已关会记 ERROR"事件丢失"，影响同旧版：最终指标快照少记一笔）。
- 交易所层与引擎层的接口：
  - `SpawnCtx { producers: Group, market_bus, account_bus, symbol_metas }`
  - `spawn_actor` 返回 `Result<ExchangeHandle, ExchangeError>`；`ExchangeHandle` 是具体类型
    （包 `Addr<ExchangeCmd>`，`enum ExchangeCmd { Subscribe(kind), SubscribeBatch(kinds), Unsubscribe(kind) }`），
    取代 `ExchangeActorOps` trait 和宏。各所节点自己收 `ExchangeCmd`。
- 外部观察者（bin 里的 FundingPoller、各 bin 的观测组件）：engine 句柄提供"往 consumers 组 spawn"的入口
  和三条总线的克隆；观察者在自己的构造闭包里 `inbox.subscribe(..)`。`Supervised` 类型门不再需要：能订阅
  就必须有 Inbox，Inbox 只能从树上的 Scope 建。
- 启动：manager 构造（预加载 metas、建总线、起全部子任务、等私有订阅确认）在 build 里完成，
  `group.spawn(..).await` 返回即启动完成；失败同时返回 Err 并上冒。
- 停机：bootstrap 收到信号或 `root.stopping()`（任何节点出错）后 `root.shutdown()`，套 30s 期限；
  `biased` 让事故分支优先（保留现有理由注释）。

## 迁移中补的公共件（先看这些，别各自再写一遍）
- `Group::spawn_addressed(name, |scope, inbox| async { ..; Ok(State) }) -> (ChildHandle, Addr<M>)`
  （`src/tasks/periodic.rs`）：有收件箱的组件一律用它起，地址在构造完成时交回。
- `Group::spawn_periodic(name, every, impl Periodic)`：定时轮询（`Periodic::tick` 做一次，Err 整支停；
  "记日志继续"的在 tick 里自己处理后返回 Ok）。首拍立即、Skip 不积压、停止优先。
- `exchange::ws_loop::WsConn`：`WsConn::spawn(&scope.group("conn"), name, read, write, keepalive)` 把读循环挂成
  子任务（断线/空闲超时 = 子任务 Err 上冒）；组件 `conn.send(text).await`、`conn.recv().await`（可放 select!）、
  私有 WS 构造时 `conn.await_subscriptions(exchange, required, confirms).await?`（确认期间的帧之后 recv 先交）。
  没有收件箱的私有 WS 停机分支先 `while let Some(raw) = conn.try_recv()` 把已到手的回报处理完。
- `exchange::{ExchangeCmd, ExchangeHandle}`（client.rs）：各所节点的收件箱消息类型就是 `ExchangeCmd`；
  公共 WS 子组件也直接收 `ExchangeCmd`。旧 `Subscribe/SubscribeBatch/Unsubscribe` 结构体已删。
- 总线别名：`messaging::{MarketBus, AccountBus}`（取代 `MarketPubSub/AccountPubSub`），出向 `engine::live::OutcomeBus`。
- 交易所层不依赖引擎层：各所节点 `XxxNode::spawn(group: &Group, args: XxxArgs) -> anyhow::Result<ExchangeHandle>`，
  `XxxArgs` 带 `market_bus/account_bus/symbol_metas` 等；`engine::live::assembly` 把 `SpawnCtx` 映射成它。
- 命名：类型去掉 `Actor` 后缀（`EquityPollingActor` → `EquityPoller`、`ExecutorActor` → `Executor`、
  `ManagerActor` → `Manager`），各所父节点叫 `XxxNode`；`XxxActorArgs` → `XxxArgs` 或直接作 `spawn` 参数。
- 参考实现：Binance 全套（`src/exchange/binance/actor/`）、`equity_polling.rs`、`crypto_status.rs`。

## 测试
- 引擎/actor 的行为测试照搬到新 API（停机顺序、出错级联、启动失败报错并级联、撤下单个策略、
  生产者收尾补发的事件送达消费者、组内并发）。专测 kameo 怪癖的测试（wait_for_shutdown 与 with_result、
  Normal vs Killed、邮箱发送端 drop）随被删的代码一起删，它们的意图已由 `tasks` 自己的测试覆盖。
- 跑：`PATH=$HOME/.cargo/bin:$PATH CARGO_TARGET_DIR=../cargo-target cargo test`；
  `cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.34 --bin funding_arb`。

## 进度（每完成一块在这里打勾并提交）
- [x] tasks 核心（tree/inbox/bus）+ 两轮审查
- [x] 交易所层：ws_loop、各所 public/private/business WS、轮询（funding_fee/listen_key/greeks/equity/crypto_status/IBKR 全部）、各所 Actor、client.rs 的 ExchangeActorOps → ExchangeHandle、assembly.rs（OKX、HL 已完成）（IBKR 已完成）（assembly 已完成，lib 编译通过）
- [x] 引擎层：bus 别名、clock、income/outcome processor、paper_counter、metrics、position_ledger、executor、supervisor、manager + provisioning、bootstrap（income_processor/executor/position_ledger 已完成）（outcome_processor/paper_counter/clock/metrics 已完成）（supervisor、manager + provisioning、bootstrap 已完成；cargo test --lib 全过）
- [ ] bins（funding_arb 的 FundingPoller 等）与 tests/
- [ ] 删 kameo 依赖、actor_lifecycle.rs、docs（framework-overview / architecture）更新
- [ ] 全量测试 + zigbuild + 审查

# TODO

## 一、spread_arb（跨所价差套利）上线前待办

按优先级排列。前四条是「上生产前建议做完」，后面是已知取舍。

### 1. 回测验证（未做）
`spread_arb` 至今没有跑过回测。现成的 `BacktestEngine`（`src/backtest/engine.rs`，基于 trades 撮合）
可以接入，需要：
- 多交易所 trades/BBO 历史数据源（当前 `BinanceHistory` 只有单所）
- 带手续费的净收益曲线，用来反推 `deviation_threshold` 的合理下界

**在此之前，`deviation_threshold` 只是拍出来的数**：`config.example.json` 里的 0.004 是占位值，
不是标定结果。该阈值必须同时覆盖双边 taker 费率 + `ioc_slippage` 让价 + 期望净收益
（配置校验只保证 `threshold > ioc_slippage`，不保证覆盖手续费——手续费不在代码里建模）。

### 2. 下单限速 / 多 WebSocket 连接（未做）
- 全链路无下单限速：`OutcomeProcessorActor` 每笔订单直接 `tokio::spawn` 打 REST。
  当前风险闸门是 `max_position_notional`（限最终暴露）+ 每 symbol 单批在途（`has_pending_orders`），
  **不限频率**。symbol 数量大时可能触发交易所限频/封禁。
- 公共行情单连接订阅全部 symbol：symbol 数量继续增长会撞上单连接订阅数上限。
  需要按 symbol 分片建多条 public WS 连接。

### 3. 单腿裸敞口只靠 `adjust_for_exposure` 中和（已知取舍）
两条腿是独立 IOC 单，可能只成交一条腿，形成裸方向敞口。当前**没有强制对冲/rebalance**：
靠后续信号在 `adjust_for_exposure` 里逐步中和，**收敛时间无上界**。
指标层已对此单独告警（`metrics.single_leg_exposure`），先靠人看。
若要收紧，可加"敞口存续超过 N 秒即强制 reduce-only 平掉"的兜底。

注：风控闸门本身不会制造裸单——`enforce_single_leg_reduces_exposure` 会把"另一腿被拦、
剩下这腿又会放大净敞口"的单整体作废；只有收敛净敞口的单腿单才放行。裸敞口的来源只有
IOC 部分/未成交。

### 3.5 风控 pipeline 建议演进为 headroom 模型
当前六个闸门都是 `&mut TradingSignal` + "size 置 0"，正确性依赖步骤顺序（`is_increasing`
对下单量不单调，所以增仓类闸门必须排在定量之后）。更干净的形态是每个闸门返回该腿的允许上限
（headroom），pipeline 取 min：顺序无关、可自由增删闸门、且**接近上限时缩量成交而不是整单拒绝**
（现在 `max_position_notional` 实际表现为"接近上限就停摆"，而不是真正的上限）。
不影响当前正确性，属结构优化。

### 4. 外部指标上报（未做）
`MetricsActor` 目前只输出结构化日志（`target: "metrics"`），无外部依赖。
若要接 Prometheus pushgateway / Slack 告警，需要新增上报组件与配置。
（历史上有过 pushgateway + Slack 实现，commit `a0183e8` 为了保持"纯策略框架"删除。）

另两点已知取舍：
- `MetricsActor` 订阅全量 income 流，数百 symbol 的 BBO 会多一份 clone + mailbox 投递，
  而它只在报告时用 mid 估值。若 CPU 吃紧，可改为报告时向 manager 取快照，或对 BBO 采样。
- 它是 `spawn_link` 到 manager 的（与其他子 actor 一致的 fail-fast 姿态），意味着观测层
  panic 会拖垮进程。定时器结束已改为只记 error 不自杀，但 panic 路径仍会级联。

## 二、框架层已知缺口

- **`min_order_size` 未参与校验**：`SymbolMeta.min_order_size` 已解析但无人使用，
  下单只校验 USD 名义值（`min_notional`）。低于交易所最小下单量的单会被交易所拒。
- **`cancel_order` 未实现**：Binance / Hyperliquid 的 `cancel_order` 直接返回 `Err`。
  `spread_arb` 只用 IOC 不撤单，但缺少"挂单卡死"的兜底手段。
- **非 `Created` 状态的挂单无超时**：`SymbolState::remove_timed_out_orders` 只清理 `Created`。
  若某订单已被交易所确认（`Pending`）而终态更新丢失，该 symbol 会被 `has_pending_orders` 永久冻结。
  当前依赖"WS 断开即进程退出 + docker 重启"来兜底。
- **`fetch_pending_orders` 对 Binance / Hyperliquid 返回空**：启动期无法同步交易所已有挂单。
- **持仓维护为纯增量**（有意为之）：`Position` 快照只做一次初始化，之后全靠 `Fill` 累加。
  不做运行期对账是为了避免快照与 Fill 流的竞态导致重复计算。代价是漏一条 Fill 则本地仓位永久漂移。

## 三、待评估的策略想法

- 资费维度（当前 `spread_arb` 完全不看资费）：开仓/平仓阈值随资费差变化——资费日化高则平仓条件更严格，
  资费低则尽快平仓；资费差低于阈值就不开仓。
- 平仓阈值独立于开仓阈值（目前只有杠杆驱动的动态阈值，没有单独的平仓阈值）。

## 四、代码整理

- `ExchangeModule` 是多余的 trait，直接用 `ExchangeClient`。

# TODO

## 一、框架层已知缺口

### 虚假 0 值 / 吞错专项审计的遗留条目
见 `docs/false-zero-audit.md` 第 5.3 节。六批修复已消灭全部 Critical 与主要 Major
（三个共享出处：`Position::checked`、`StalenessGuard`、`validate_subscriptions` 的
SymbolMeta 判据，加复审补的 `ibkr/wire.rs`）。遗留条目里 IBKR 的几项随 IBKR 适配层删除（#816）作废，
其余按需另立任务。第 5.2 节记了一处**有意不改**的
复审建议（`unrealized_pnl` 保持强制）及其观察方式。

- **`min_order_size` 未参与校验**：`SymbolMeta.min_order_size` 已解析但无人使用，
  下单只校验 USD 名义值（`min_notional`）。低于交易所最小下单量的单会被交易所拒。
  同一问题的另一面：数量按 `size_step` 向下取整可得 0，全链路无人拦截，会发出 qty=0
  的单进入"被拒 → 重试"的静默循环。
- **事件分发粒度只到 (所, symbol)，不看订阅 kind**：只订 BBO 的策略照收该 symbol 的
  Trades/MarkPrice 等全部事件。收紧到 kind 有行为风险（策略消费了未声明的 kind 会静默
  丢事件），需要配套：策略声明修正（策略消费的 kind 都要声明）+ 回测 accepts 同口径收紧，作为一个整体任务做。
- **回测引擎不做下单量下界校验**：`SymbolMeta::checked_exchange_qty` 已是唯一出处，
  实盘出口与模拟柜台都接了，但 `BacktestEngine` 没有 SymbolMeta 来源 —— 实盘必拒的
  碎单在回测里照样成交。根治方向是把校验下沉到 SimState 的订单入口（构造时可选携带
  该所 metas），让三个持有方自动同规则，而不是每个持有方各自记得调用。
- **挂着的单终态丢失无超时**：订单跟踪表对「下单没结论 / 撤单发出后没终态 / 终态后成交没到齐」超过
  1 分钟停机（`ORDER_STUCK_MS`，#758），但一张挂着、没撤过的单若终态回报丢失，会一直留在表里。
  当前依赖"WS 断开即进程退出"兜底；策略撤它时会落进「撤单发出后没终态」被发现。
- **持仓维护为增量 + 独立对账通道**：`Position` 基线一次性写入（manager 投产期点对点 +
  总线各一条路径），之后全靠 `Fill` 累加；`PositionLedger` 周期性拉 REST 读数，
  与「基线 + Fill」比对，连续差值稳定即停机。对账**只检测不修复**
  （无 re-baseline / 自动平仓），模拟账户与挂单不在对账范围内。漂移的 error 日志现在会经
  告警外送通道推 webhook（若配置了 `ALERT_WEBHOOK_URL`）。

已解决（记录供回溯）：
- ~~`cancel_order` 未实现~~：四所均已实现（`e6348f1` 补齐 Binance / Hyperliquid / IBKR）。
- ~~`fetch_pending_orders` 对 Binance / Hyperliquid 返回空~~：已实现（`e06133c`），
  启动期/降级时用于撤掉本引擎遗留挂单。

## 二、代码整理

- `ExchangeModule` 是多余的 trait，直接用 `ExchangeClient`。

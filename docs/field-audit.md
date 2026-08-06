# 域模型字段审计：谁在为无人读的字段付对齐成本

> 审计日期：2026-08-06，基于 `feat/position-reconcile`。
> 起因：持仓的均价与盈亏"策略用不到，还要费劲对齐各个交易所"。顺着这条线索把
> `src/domain/models/` 全部结构体过了一遍，逐字段追到真实读者。

## 判定标准

**算真实读者**：策略决策、风控闸门、撮合逻辑、状态维护、回测结果、supervisor 判据、
指标/告警输出。
**不算**：仅被赋值/构造、仅出现在 `tracing::` 日志插值、仅在测试里断言、仅被 Debug 打印。

跨层要追到底：字段进了某个 map，就继续追那个 map 的 getter 有没有调用方。

## 一个决定性的全局事实

**全仓库唯一被订阅的 `SubscriptionKind` 只有 `BBO` 与 `Trades`。** 没有任何策略或 bin
订阅 `FundingRate` / `MarkPrice` / `IndexPrice` / `Candle` —— 这四条通道处于"有生产者、
无消费者"状态：四所的解析、订阅报文拼装、`SymbolState` 缓存、getter 全部空转。

---

## 已删除（三轮，见下方提交）

| 目标 | 为它付的对齐成本 | 提交 |
|---|---|---|
| `Position.entry_price` / `unrealized_pnl` | 四所各报各的（`avgPx`/`entryPx`/`avgCost`/`entryPrice`），空仓表达还各不相同（空串/null/缺键）；为对齐它们建了 `Position::checked` 与三所各自的三态守卫 | `7b214a5` |
| `OrderUpdate` 的 `price`/`reduce_only`/`filled_quantity`/`fill_sz`/`timestamp`，及 `OrderStatus::PartiallyFilled` 的载荷 | 见下节 | `392dd21` |
| `Balance.frozen` + `total()`；`Greeks.gamma/theta/vega` | Binance 凭空推导 frozen、HL/IBKR 写死或不发；OKX 的三个希腊值 parse + 回测源为对齐它们写的两处单位换算 | `d0cec70` |

净减约 400 行。四个适配层的持仓解析都只剩"取数量"，挂单回报只剩"跟踪一张单必需的字段"。

### `OrderUpdate` 五个字段的删除理由（最典型的对齐税）

- **`fill_sz`（本次成交量）**：Hyperliquid **没有这个概念**，适配层拿累计量冒充（已知失真）；
  IBKR 的 sor 推送也没有，三处写死 0。一个"四所里两所在编、零读者"的字段。
- **`reduce_only`**：OKX 要三态字符串解析（`Option<String>` + `== "true"`）、REST 与 WS
  口径还不一致；HL/IBKR 共四处写死 `false` 外加四条解释性注释。文档曾声称它用于"重连时
  准确注册现存挂单"，但**没有消费方读过重建后的这个值**。
- **`timestamp`**：OKX/Binance 填本地墙钟、HL 填交易所时间、IBKR 填接收时刻——两种语义
  混装，而唯一写入落在一条**永不可达**的超时分支上（超时清理只作用于 `Created` 态，而
  重建进来的必是 `Pending`/`PartiallyFilled`）。
- **`filled_quantity` + `PartiallyFilled` 载荷**：必须一起删才有意义，否则各所的累计量
  解析原样保留（OKX 的张→币折算、HL 的 `origSz - sz`、Binance 的 `z`、IBKR 的三态取数）。
- **`price`**：唯一"读者"是一条为 IBKR 写死 0 而设的空值守卫——守卫判 `quantity` 即可
  （幽灵挂单的数量同样是 0），防线一点没弱。

---

## 待决策：整条通道死透的四组（S 级）

这些**不是无用字段，而是暂未使用的框架能力**。删掉是减少能力而非减少负担，故列出待定，
并给出我的建议。

| 通道 | 现状 | 删除收益 | 我的建议 |
|---|---|---|---|
| `FundingRate`（含 `daily_rate*`、`unified_time_base`、`best_short/long_exchange`） | 3 所 codec + 6 处订阅/dispatch + `SymbolState` 约 60 行死逻辑全部零读者 | 高。⚠️ HL 专门合成 `next_hourly_settle_time`（交易所不给结算时刻）、两处 `.max(1.0)` 小时钳制都是为跨所日化可比写的 | **保留**：`docs/todo.md` 第三节明确有"资费维度"策略计划（资费日化高则平仓更严格），删了要重写 |
| `FundingFee`（含凭空合成的 `tran_id`） | 两个完整 polling actor 零读者 | 中。⚠️ HL 没有原生 ID，`tran_id` 是 `md5(time, coin)` 合成的，注释声明的"下游去重"不存在 | **与 FundingRate 一起决定**：资费策略要做，流水是它的账 |
| `MarkPrice` + `IndexPrice` | 6 份 codec + 6 处 dispatch + 2 个订阅 kind + 两个死 getter | 中，且是**干净删除**（无特殊对齐逻辑） | **可删**：重写成本低（每所约 20 行），要用时再加 |
| `Candle` + `CandleInterval` + `HistoryCandles` | OKX 独家实现，但**整条 business WS 通道基本只为它存在** | 高 | **可删**，但先确认没有 K 线策略计划 |
| `MarketStatus` | `crypto_status.rs` 整个文件 + IBKR `status_polling` 零读者 | 中 | **保留**：IBKR 股票的交易时段判断是安全机制（非交易时段不下单），当前无人读是缺口而非冗余——应该补上消费方 |
| `BorrowFee` + `ExchangeRate` | IBKR snapshot 两条轮询 + staleness guard 零读者 | 中 | **保留**：为融券做空策略准备的数据源，删了该策略就没法做 |

## 其余单字段候选（A/B 级，尚未删）

| 字段 | 结论 | 建议 |
|---|---|---|
| `MarketTrade.is_buyer_maker` | 零读者，且 `sim/matcher.rs:28` **明确声明不用它**，而 `market_trade.rs` 的文档还在说它是"回测撮合的成交来源"——文档与实现矛盾。四所各一套 maker 约定（OKX/HL 各自反推、Binance 直取、IBKR 写死）+ 回测 CSV 两个数据集列序不同的坑 | **建议保留字段、修正文档**：主动买卖压是有价值的微观结构信号，删了将来做订单流策略要重来；但那条矛盾的文档必须改 |
| `Fill.client_order_id` / `order_id` | 零逻辑读者（`fill.rs` 注释"用于关联 PendingOrder"是未兑现承诺） | **保留**：诊断价值实在（"这笔成交对应哪张单"），且四所本就提供，无对齐成本 |

## 不要删（与初始怀疑相反）

1. **`SymbolMeta.min_order_size` 有真实读者**：`checked_exchange_qty` 的下界校验，经实盘
   出向唯一出口与模拟柜台双路径生效。它防的是"下单→被拒→pending 清除→重下"的静默重试环。
   这一项是**该用而且在用**，不是无用。
2. **`Fill.timestamp` 有真实读者**：supervisor 的 `RoundTrip.opened_at/closed_at` →
   `round_trips()` 是 `PromotionPolicy` 扩展点的契约面。内置判据是 `NeverPromote`（有意
   留白），但这是扩展点而非死代码。
3. **`BBO.bid_qty` / `ask_qty`**：spread_arb 用盘口深度限制下单量（`ORDERBOOK_TAKE_RATIO`）。
   OKX 为它做的张→币折算是必要对齐。

## 顺带发现的死 getter（零调用方，可清入口）

`StateManager`：`total_equity()`、`account_notional()`、`total_account_notional()`、
`market_status()`。
`SymbolState`：`best_short_exchange()`、`best_long_exchange()`、`mark_price()`、
`index_price()`、`has_pending_side()`。

---

## 复盘：为什么这些字段会长出来

三轮删除下来，同一个模式反复出现：**结构体承担了两个角色**。

`Position` 混了"交易所上现在持有多少"（决策要的）与"这笔仓位成本多少"（记账要的）；
`OrderUpdate` 混了"这张单还在不在"（跟踪要的）与"它成交了多少、什么价"（对账/统计要的）。
一旦混在一起，交易所适配层就被迫解析本地才需要的字段——而各所对那些字段的支持程度天差
地别，于是"写死 0""拿别的量冒充""三态字符串解析"就都来了。

判据可以简化成一句：**一个字段如果四个交易所里有人给不出来、而本地又没人读，它就不该在
域模型里**。给不出来说明它不是这个层次的事实，没人读说明它不是需求。

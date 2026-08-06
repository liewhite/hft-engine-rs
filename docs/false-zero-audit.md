# 虚假 0 值与吞错专项审计

> 审计日期:2026-08-06,基于分支 `feat/position-reconcile`(746e402)。
> 范围:四个交易所适配层(Binance/OKX/Hyperliquid/IBKR)+ 核心引擎(messaging/sim/engine/backtest)。
> 每条发现均已追到下游消费者,行号以审计时点为准。

## 四条判定原则(修复的统一基准)

1. **诚实**:某个字段可以没有,但不能是假的。拿不到就是拿不到(`Option::None` / `Err`),绝不编一个 0、空串或猜出来的值顶上。
2. **fail-fast**:失败要立刻、大声地失败。解析不了私有回报、确认不了订阅、对不上归属——挂掉重来,不带病运行。
3. **严格区分 Optional 和 0**:`None` 是"不知道",`0` 是"知道且为零"。交易所语义里的合法 0(空仓、无手续费)才允许是 0;解析失败、字段缺失兜出来的 0 一律非法。
4. **能靠 fail-fast 兜底的就不必层层设防**:不在每个消费点加检查,在数据入口一次把关,过不了就挂。出错就挂没问题——半吊子的检查(检出了也只 warn)比没有更糟,它制造虚假的安全感。

三个系统性模式贯穿了大多数单点问题,修复应对模式下手而非逐点打补丁:

- **模式甲**:"解析失败"与"合法缺失"混进同一个 0(违反原则 1+3);
- **模式乙**:快照/列表逐条 skip-continue,"缺了一条"伪装成"干净"(违反原则 2)——消费方把"不在列表里"推断为"已终态/空仓",一条被 skip 的记录就变成误判;
- **模式丙**:订阅/会话失效无确认机制(违反原则 2)——连接活着 ≠ 订阅活着,私有流断了系统照样全绿。

---

## 一、违反「诚实」:值是编造的

### 1.1 OKX 挂单快照编造 client_order_id 【Critical】
`src/exchange/okx/client.rs:412-417`:clOrdId 缺失时填 ord_id,`client_order_id` 永不为 `None`。

下游:撤单复查 `cancel_recheck_verdict`(`src/engine/live/outcome_processor.rs:344-361`)的 `Unverifiable` 安全分支判据是 `is_none()`——该安全网在 OKX 上**结构性失效**,无法比对身份的活单会被判 `Gone`、合成 Cancelled 误清本地 pending → 策略重复下单、双重敞口。次生:`own_pending_orders` 按前缀判归属,ord_id 无前缀 → 本引擎的单被判"别人的",永久留在交易所。

**修法**:缺失就是 `None`(原则 1);安全分支本来就是为 `None` 设计的,让它工作。

### 1.2 IBKR notional 解析失败编 0 → 杠杆闸门恒通过 【Critical】
`src/exchange/ibkr/client.rs:640-645`:`grosspositionvalue` 解析失败 → warn + 0.0。

下游:`AccountInfo.notional=0` → `strategy/spread_arb` 的 `notional/equity=0` → `max_account_leverage` 闸门**恒通过**,无论真实杠杆多高。注意与 equity 的不对称:equity 兜 0 是安全侧(策略判净值不足不下单),notional 兜 0 是**危险侧**(放开加仓)。

**修法**:解析失败 → `Err`(原则 2,调用方 account_polling 已有 Err 分支);或 `AccountInfo` 字段改 `Option` 让"未知"可表达(原则 3),策略现有的"notional 未到不下单"分支自然接住。

### 1.3 持仓均价三所同款:解析失败兜 0 进基线 【Major,三处】
- OKX `src/exchange/okx/codec.rs:275-289`:`parse_or_zero` 逐字段独立兜底——`pos="12"` 而 `avgPx=""` 时产出 size 非 0、entry_price=0 的持仓,而非报错;
- IBKR `src/exchange/ibkr/client.rs:220-233`:`avgCost`/`unrealizedPnl` 解析失败 warn + 0;
- HL `src/exchange/hyperliquid/codec.rs:408-414`:同文件 `szi`/`upl` 都上抛,唯独 `entry_px` 的 `Some(p)` 解析失败兜 0("空仓可为 None"的合法语义被顺手扩大到了解析失败)。

下游:`PositionBaseline.entry_price=0` → `Position::apply_fill` 从 0 起算加权均价、首笔反向成交算出巨额虚假 realized → 策略止盈/风控读烂数据。**且永不可纠正**:基线每 (所,symbol) 只写一次,第二次到达判违约;对账只比 size,看不见 entry_price 错误。

**修法**(统一口径,原则 3):只有 size 解析为 0 时才允许价格类字段空缺兜 0;size 非 0 而价格字段缺失/解析失败 → `Err`。

### 1.4 OKX fee 取的是订单累计手续费,不是本次成交手续费 【Major】
`src/exchange/okx/codec.rs:427`:解析 orders 频道的 `fee`(累计值),正确字段是 `fillFee`(单笔)。全仓库无人解析 `fillFee`。⚠️ 字段语义需对照 OKX v5 文档最终确认。

下游:一张分 3 次成交、每次 0.1 的单被记成 0.6(N 笔均等成交高估 (N+1)/2 倍)→ `TradingStats` 净利错 → **污染 supervisor 的模拟→实盘晋升决策**。且 Binance 的 `n` 是单笔口径,两所反向不一致。

### 1.5 IBKR commission 到了但解析失败 → 填 0 且日志谎报 "received" 【Major】
`src/exchange/ibkr/actor/public_ws.rs:508-520`:第二条消息已到、`parse_ib_number` 失败 → `unwrap_or(0.0)`,INFO 日志断言 "commission received"。超时路径反而有 WARN。下游:手续费/净利长期低估,且伪装成成功。

### 1.6 IBKR 非美股 SymbolMeta 是编造的常量 【Major】
`src/exchange/ibkr/client.rs:348-362`:price step 0.01、size_step 1.0 全所硬编码,注释称"IBKR 股票精度固定"。但本适配层显式支持非美市场(韩股 fallback、KRW 汇率轮询),KRX 的价位是按价格分档的整数韩元。`ExchangeOrder::from_domain` 按假 meta 取整 → 交易所必拒。

### 1.7 OKX 私有事件 exchange_ts 一律填本地时间 【Major】
`src/exchange/okx/actor/private_ws.rs:337,345`、`codec.rs:406,445`:`uTime`/`fillTime` 被丢弃。下游:OKX 腿的端到端时延观测恒为 0,WS 堆积/延迟故障不可观测;RoundTrip 持仓时长变本地口径。

### 1.8 轮询失败后,陈旧值伪装成新鲜值 【Major】
`binance/actor/equity_polling.rs:62-69`、`binance/actor/funding_fee_polling.rs:64-71`、`okx/actor/greeks_polling.rs:68-75`、`ibkr/actor/account_polling.rs:61-68`、`ibkr/actor/status_polling.rs:46-53`:失败只 warn,不计连续失败、无停摆守卫。

下游:`StateManager.account_infos` 保持旧值,`equity()` 返回 `Some(旧值)`——**equity 可无限期陈旧,杠杆闸门用旧数而无从分辨**。对比:position_polling 有 `MAX_POLL_STALENESS_MS` 致命线,姿态不一致。

**修法**(原则 4,不做时效戳那套复杂设计):连续失败 N 次 → 致命退出,与 position_polling 同款。

### 1.9 小项
- Binance `Balance.frozen = (wb-cw).max(0)` 是推导伪 0,且 `available` 在 Binance(crossWalletBalance)与 OKX(cashBal)间口径不同源,跨所汇总是混口径相加(`binance/codec.rs:186-200`)【Minor】。
- Binance `InsufficientBalance(_, 0.0, 0.0)` 金额编 0,仅日志展示(`binance/client.rs:731`)【Minor】。
- IBKR BBO 的 bid/ask size 未收到时以 0 发布(`public_ws.rs:271-320` 增量缓存默认值):"没收到"混同"盘口无量"→ 下单量算 0 被下界校验拦下,安全侧但归因误导(报订单校验失败而非数据缺失)【Major→观测面】。
- OKX 错误码非数字兜 -1(`okx/client.rs:302` 等),仅观测值【Minor】。

---

## 二、违反「fail-fast」:该挂不挂,带病运行

### 2.1 OKX 私有回报缺 SymbolMeta 整条丢弃 【Critical】
`src/exchange/okx/actor/private_ws.rs:321-329`:`resolve_meta` 失败 → warn + continue,**Fill 与 OrderUpdate 一起丢**。

触发链(现实):`get_all_instruments`(`okx/client.rs:160-180`)用 `filter_map` **静默剔除**任何 `tickSz/lotSz/minSz/ctVal` 解析失败或 ≤0 的合约(无日志)——而私有 WS 订的是全账户全量。该 symbol 持仓基线可能已建立(基线走另一份缓存),Fill 却全丢 → 本地持仓落后于交易所 → 对账连续失配**致命停机**(有兜底,但表现为停机而非纠正);丢 OrderUpdate → pending 永不清除 → symbol 永久冻结。

**修法**:元数据加载失败改为可见(至少 error);私有回报"有基线却无 meta"升级为 `Err`(kill)。多桶部署下"别的桶的 symbol"与"元数据丢了"必须可区分。

### 2.2 OKX 挂单快照逐条 skip → "缺失"伪装"干净" 【Critical】
`src/exchange/okx/client.rs:381-410`:inst_id/side/acc_fill_sz/state 解析失败 → warn + continue;同函数 px/sz 却是 `return Err`——自相矛盾。

下游一(启动期):被 skip 的单不在快照 → `cancel_leftover_orders` 判"已撤净"**假通过** → 带着无人看管的活单开跑(该函数注释明写绝不允许)。
下游二(运行期,更险):撤单失败复查时该单不在 open_orders → 判 `Gone` → 合成 Cancelled **误清活单** → 重复下单、双重敞口。

**修法**(模式乙的统一修法):快照类接口一条解析不了 = 整个快照不可信 = `Err`。消费方(启动撤单、撤单复查)对 `Err` 已有正确分支(保留 pending / 拒绝启动)。附带:`orders-pending` 未分页(>100 条截断,同链条)【Minor】一起修。

### 2.3 IBKR 私有成交解析失败静默丢弃 【Critical】
`src/exchange/ibkr/actor/public_ws.rs:564-570`(price/size 解析不出 → warn + continue)、`:491-499`(execution_id 缺失 → continue)。守卫本身对(拦住了假 0),但**丢弃整笔 Fill** 比放进假 0 同样致命:本地持仓从此落后,靠对账延迟停机兜底。同模块"预热失败即 kill"的 fail-fast 姿态没有覆盖最重要的数据。

**修法**:解析失败 → `Err(WsError::ParseError)`(kill),与本模块既有姿态同口径。

### 2.4 Hyperliquid 订阅被拒完全无声 【Critical】
`src/exchange/hyperliquid/actor/private_ws.rs:196-226`、`public_ws.rs:361-497`:HL 的错误格式是 `{"channel":"error","data":"..."}`,现有两处 error 检查(顶层 `error` 键、subscriptionResponse 里的 `data.error`)位置都不对,是**死代码**;真实错误落进 `_ => debug!`。private_ws 连这两个检查都没有。

后果:orderUpdates/userFills/clearinghouseState 任一订阅被拒(地址、dex 参数、限流),keepalive 照样 ping/pong 判健康 → **私有流全断而系统全绿**。

**修法**(原则 4,简单记账不做重试):`"error"` channel → `Err`(kill);subscriptionResponse 改记账式确认,N 秒内未集齐全部订阅确认 → kill。

### 2.5 IBKR 持仓逐条 continue → 基线污染且对账失明 【Critical】
`src/exchange/ibkr/client.rs:198-218`:conid/position 解析失败 → warn + continue。致命组合:**基线与对账读数共用同一解析函数**——字段名/类型一变,基线 0、读数也 0 → 对账判"一致",漂移检测结构性失明;基线只有一次机会,永不可纠正。整体守卫(响应非数组即错)有了,逐条守卫没有——对照 HL 的"过滤后一个不剩即报错"(`hyperliquid/client.rs:772-786`,本仓库的正确范式)。

**修法**:逐条解析失败 → `Err`,与 HL 对齐。

### 2.6 IBKR REST 挂单归属键疑似取错字段 【Critical,需实盘确认】
`src/exchange/ibkr/client.rs:435-437`:REST 解析只认 `cOID`;同仓库 WS 侧两处注释明写 cOID 以 **`order_ref`** 回传(`public_ws.rs:331,344-345,549-550`),REST 无兜底。若 live-orders 确实回 `order_ref`,则 `coid` 恒 `None` → `own_pending_orders` 全部"无法确认归属,保留不动" → 启动期**一张遗留单都撤不掉且谎报已撤净**;撤单复查侧则全 `None` + 空 order_id → `Unverifiable` → symbol 冻结。

**修法**:先用 `tests/ibkr_test.rs:473` 的现成打印跑实盘确认字段名;无论真相如何,`#[serde(alias = "order_ref")]` 双取 + 把真实响应固化成测试(照 HL `REAL_CLEARINGHOUSE_STATE` 范式)。

### 2.7 其余 fail-fast 违反 【Major/Minor】
- **核心** `src/engine/live/executor.rs:51`:策略信号发布失败 `let _ =` **静默丢弃**——丢的是下单信号,至少 error 日志(总线挂掉本就该整机退出,让失败可见即可,原则 4)【Major】。
- IBKR conid 解析失败静默剔除 symbol(`symbol.rs:104-110` → `client.rs:348-353` filter → 订阅时 ignore):配置了的 symbol 行情永远不来,启动全绿。`validate_subscriptions` 管不到 conid【Major】。
- IBKR 未处理 `sts`(authenticated=false)/system topic(`public_ws.rs:236-260` 一律 debug 吞掉):模块文档花 20 行讲"静默断流",唯一能暴露它的信号没接【Major】。
- IBKR `pending_fills` 两条无声丢失:`public_ws.rs:620-627` 的 `let _ = tell(FillCommissionTimeout)`(失败则该 fill 永不发布);`on_stop` 不 flush(停机时 3 秒窗口内的成交蒸发)【Major】。
- IBKR fill 一致性检查名存实亡(`public_ws.rs:522-533`):缺字段兜 0 导致每笔误报训练运维忽略,真检出不一致也只 warn 照旧发布。**要么检出即挂,要么删掉**(原则 4:半吊子检查比没有更糟)【Major】。
- IBKR `build_order_update`(str 路径)只按 u64 解析 orderId(`public_ws.rs:646-650`),同文件 sor 路径 30 行前刚修过双态并写了注释——已知坑的遗漏修复【Major】。
- Binance 元数据 step 字段解析失败静默剔除(`binance/client.rs:233-246`,无日志;后果比 OKX 轻:订阅时有 warn,私有路径不查 meta)【Minor】。
- HL 未知订单状态一律映射 `Rejected`(`hyperliquid/codec.rs:495-509`):`marginCanceled` 等撤单语义被计入拒单统计,终态处理相同故不影响 pending【Minor】。
- IBKR ssodh/init 不检查 2xx(`client.rs:47-56`);HL funding 轮询失败无守卫;OKX 公共/business WS 未知 event 连 warn 都没有(私有侧有)【Minor】。

---

## 三、违反「区分 Optional 和 0」:类型层面的混同

### 3.1 Binance 回报 schema 把可缺失字段声明为必填 【Major】
`src/exchange/binance/codec.rs:232-234`:`n`(手续费)、`ap`、`rp` 是 `String` 必填。官方文档:无成交时**不推** `n`。一条合法的 NEW/CANCELED 回报 → 整条反序列化失败 → `WsError::ParseError` → kill → **整机停机**。

这是 fail-fast 的镜像错误:原则 1 说"字段可以没有"——schema 必须如实声明 Optional。加 `#[serde(default)]` 安全:`to_fill` 在 `fill_sz==0` 时提前返回,`n` 只在真有成交时被读。

### 3.2 IBKR LiveOrder 的宽容声明与实现相反 【Major】
`src/exchange/ibkr/client.rs:425-450`:文档说"字段一律 Option/default,不该让整条响应解析失败";但 `#[serde(default)]` 只管缺失不管类型不符——`price: Option<f64>` 遇到字符串 `"155.69"` 整条失败 → 启动被挡死。同 crate 已有 `parse_ib_number` 双态范式,`price/totalSize/filledQuantity` 应统一。
(注:`:517 price.unwrap_or(0.0)`、`:520 total_size.unwrap_or(0.0)` 本身良性,下游有守卫,见附录 A.1。)

### 3.3 修法基线
凡"交易所可能不给"的字段:领域模型用 `Option` 承载或在解析处区分三态(缺失=合法语义/在但解析失败=Err/在且有效=值)。禁止 `unwrap_or(0.0)` 出现在解析失败分支——它只允许出现在**有注释契约的合法缺省**上。

---

## 四、按「原则 4」明确不做的事

- **核心 `SymbolState` 的 Fill 入口不加 price/size 守卫**(`messaging/state.rs:393` 直接喂 `apply_fill`):正确修法是上面的适配层入口把关(2.3/2.5),数据进了域就默认可信。层层设防会把不变量的责任摊稀。
- **轮询时效不做时间戳穿透设计**:连续失败 N 次 kill(1.8)足够,不给每个读数附时效戳。
- **HL 订阅确认不做重试/降级逻辑**:超时未确认直接 kill(2.4),重启即重订阅。
- **对账通道维持"只检测不修复"**:它是 fail-fast 的最后兜底,不因本次审计给它加自动纠正。

---

## 五、修复记录（计划内条目已完成；遗留清单见 5.3）

| 批次 | 内容 | 对应条目 | 风险主题 | 提交 |
|---|---|---|---|---|
| 1 | OKX 挂单快照 skip→Err + 不再编造 clOrdId + 补分页 | 2.2、1.1 | 误清活单/双重敞口 | `16bf971` |
| 2 | HL error 频道 + 订阅记账确认;IBKR 成交解析失败致命 | 2.4、2.3 | 静默断流 | `fa559a5` |
| 3 | `Position::checked` 统一口径;IBKR 持仓逐条守卫 | 2.5、1.3 | 基线污染(永不可纠正) | `d2528a0` |
| 4 | IBKR notional → Err;`StalenessGuard` 停摆守卫 | 1.2、1.8 | 风控失明 | `1b5ab43` |
| 5 | 投产期校验 SymbolMeta;元数据剔除留痕 | 2.1 | Fill 丢失 | `2c85220` |
| 6 | OKX fillFee;Binance schema;IBKR order_ref/orderId;executor 日志 | 1.4、3.1、2.6、2.7 | 其余 | `5419d55` |

### 修复中确立的三个共享出处（避免同类问题再次分散复发）

- **`Position::checked`**（`src/domain/models/position.rs`）：持仓非零时价格字段必须有值。
  四个适配层共用，调用方不必各自记住规则。
- **`StalenessGuard`**（`src/exchange/staleness.rs`）：周期性读数的停摆守卫。账户净值 /
  希腊值 / 持仓对账四处共用，`position_polling` 原有的手写实现已迁移过来。
- **`validate_subscriptions` 的第三个判据**（`src/engine/live/manager.rs`）：订阅的
  (所, symbol) 必须有 SymbolMeta。把"元数据被剔除"从三种分散症状收敛成投产期的一次判定。

### 5.1 复审补做的一批（`16b2758` 及后续）

第一轮复审(0 Critical)指出三个 Major，都是"修了 A、同类 B 没修"的残留，已补：

- IBKR 的 Number|String 双态解析有**三份手抄**（两条 WS 路径 + REST，行为还不一致）→
  收敛为 `src/exchange/ibkr/wire.rs`（第四个共享出处）。顺带修掉旧写法把 `null` 变成
  字符串 `"null"` 的编造问题——它会绕过撤单复查对空 id 的保守分支。
- IBKR REST 挂单快照：`price/totalSize/filledQuantity` 从 `Option<f64>` 改双态解析
  （`serde(default)` 只兜字段缺失，类型不符照样让整条响应失败 → 启动被格式差异挡死，
  即条目 3.2）；`side` 认不出由 skip 改 Err（模式乙在 IBKR 侧的同款残留）。
- `pending_fills` 两条丢失路径：commission 超时消息的 `let _ = tell` 改 error 日志
  （它是该 fill 唯一的兜底发布路径）；`on_stop` 补 flush（已发生的成交不能随停机蒸发，
  否则下次启动的基线会把缺口固化）。
- HL 订阅记账改为"回执**或**该频道数据到达"两个等效判据：原实现依赖"HL 一定对三条
  订阅都回 `subscriptionResponse`"这个未经实盘验证的假设（`clearinghouseState` 不在
  HL 公开文档的订阅主列表里），假设错则上线即 15 秒自杀循环。数据到达是更硬的证据。

### 5.2 一处**有意不改**的复审建议

复审建议把 `unrealized_pnl` 从 `Position::checked` 的强制项里拆出（理由：uPnL 每轮
重算可自愈，而 IBKR 在无行情时段可能对非零持仓回 `null`，会导致对账持续 Err → 60 秒
后整机退出）。**不采纳**：兜 0 就是假值，与原则 1 直接冲突；而该场景是未经证实的推测，
若真发生，失败是响亮的（error 日志直接指明字段与 symbol），几分钟内即可定位并按实际
行为调整——这比现在就为一个假想场景放宽规则更符合原则 4。列为上线初期观察项。

### 5.3 遗留未修条目（本次六批**不含**，按需另立任务）

| 条目 | 内容 | 未修原因 |
|---|---|---|
| 1.6 | IBKR 非美股 SymbolMeta 硬编码（price step 0.01 等） | 需要接 IBKR 合约详情接口取真实精度，是新增能力而非修补 |
| 1.7 | OKX 私有事件 `exchange_ts` 填本地时间（丢弃 uTime/fillTime） | 影响面是时延观测，不影响账本；改动需同时校准所有时延指标 |
| 1.9 | Binance `Balance.frozen` 推导伪 0；`available` 两所口径不同源 | `total()` 目前无调用方；跨所汇总的口径统一应与账户模型一起做 |
| 2.7 残留 | IBKR 未处理 `sts`(authenticated=false)/`system` topic | 需要确认 IBKR 各 topic 的实际报文形态，宜与一次实盘窗口一起做 |
| 2.7 残留 | IBKR conid 解析失败静默剔除 symbol | 投产期已由 `validate_subscriptions` 的 SymbolMeta 判据间接拦住；剩下的是加载处的日志留痕 |

### 与原报告的两处判断修正

- **2.6（IBKR 归属字段）**：未做实盘确认，改为 `#[serde(rename = "cOID", alias = "order_ref")]`
  双取。无论真相如何都正确，且不必等一次实盘窗口。
- **2.1（OKX 缺 meta 丢私有回报）**：根因不在 OKX 私有流，在投产期校验缺了一条。修好
  上游后，私有流那条丢弃分支只剩"该 instId 不归本实例管"一种解释（全账户推送里的人工单 /
  别的分桶实例），丢弃是正确处理，故降为 debug 日志。

---

## 附录 A:已核实的合法 0 / 正确姿势(勿重复排查)

1. **合成回报的 0 字段**(撤单确认 price/quantity=0、REST 快照 fill_sz=0):有契约注释;且 `OrderUpdate.fill_sz`/`filled_quantity` 全引擎无读取方(已 grep 核实),pending 完全由 client_order_id + status 驱动。
2. **`SymbolState` 挂单重建守卫**(`state.rs:320-334`):注释点名 IBKR 的 0 值推送,"宁缺勿假",有回归测试——正是原则 1 的正面示例。
3. **对账"快照里没有 = 空仓"**(`position_reconcile.rs:249`):全量快照契约成立的合法推断,变体文档写明。
4. **单边盘口 `Ok(None)`**(OKX/HL codec):合法市场状态,有注释与单测。
5. **无凭证 `fetch_positions` 返回空 Vec**:"没账户 = 空仓是事实",契约注释划清了 place/cancel 不适用。
6. **OKX/HL `fetch_account_info` 显式 `Err`**:拒绝编造 notional=0,正是 1.2 应有的姿势。
7. **HL 私有 WS 解析失败全部 `?` 上抛 kill**:2.3 应对齐的范式。
8. **HL "过滤后一个不剩即报错"守卫 + 真实响应固化测试**:2.5/2.6 应对齐的范式。
9. **双向持仓模式两所都在解析处拒绝**(不猜方向),有单测。
10. **IBKR smd 刷新机制、snapshot required 字段、eval_stale 冻结豁免**:对"静默断流"的处理标杆(讽刺的是 2.4 的 HL 与 M8 的 sts 没享受同等待遇)。
11. **Binance codec 数值字段全部 `Err` 传播**(无一处 unwrap_or);缺 `E` 回落 local_ts 带 warn,是显式降级非静默。
12. **Binance/OKX 不消费持仓快照推送**:与"基线 + Fill 累加"模型一致。

## 附录 B:待外部确认的两点

- 2.6:IBKR `GET /iserver/account/orders` 回传的归属字段是 `cOID` 还是 `order_ref`(实盘打印确认)。
- 1.4:OKX v5 orders 频道 `fee`(累计)与 `fillFee`(单笔)的语义(对照官方文档确认)。

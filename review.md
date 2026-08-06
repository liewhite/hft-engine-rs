
---
一、正确性（阻塞级）

1. SimState / Ledger 根本没有交易所维度，但接口假装有

src/sim/ledger.rs:9、src/sim/state.rs:51-54

pub positions: HashMap<Symbol, Position>,        // Ledger
pub resting:   IndexMap<OrderId, RestingOrder>,  // SimState，RestingOrder 无 exchange 字段
pub last_bbo:  HashMap<Symbol, BBO>,             // last_mark / last_trade 同

而 apply_fill(exchange, ...)、on_market(exchange, ...)、on_order_arrived(.., exchange, ..) 全都收 exchange 参数 —— 只用来填 Position::empty 和给回报事件贴标签，不参与任何键。

后果：fill_crossed（state.rs:147）只按 symbol 过滤，Binance 上的挂单会被 OKX 的 trade print 撮合，成交回报还被打上 OKX 的标签；两所持仓合并成一条。PaperCounterActor 订阅的是全所共享行情（manager.rs:484），一个账户一份 SimState（paper_counter.rs:73），所以只要有一个跨所策略绑到 paper 账户，账本立刻串味。

今天没炸只是因为 paper_trade / adaptive_trade 都写死单所、BacktestEngine 也只持有一个 exchange。类型和签名里没有任何东西挡住这件事。

顺带：paper_counter.rs:26-28 的模块文档写"每个交易所各自持有一份 SimState……持仓与净值按所隔离"，和 line 73 的"按账户而非按交易所分账"直接矛盾。文档是错的。

2. 柜台净值事件写死 Exchange::Binance

src/engine/live/paper_counter.rs:251

self.publish_account_info(ev.local_ts, Exchange::Binance).await;

AccountInfo 在 StateManager 里是按 exchange 建键的（state_manager.rs:196），策略经 equity(exchange) 取。跑 OKX/HL 模拟盘时（paper_trade.rs:139-144 明确支持），柜台发的是 Binance 的净值，策略查自己所的 equity 恒为 None → require_equities 一路清零 → 永不下单，且只在 warn 日志里留痕。注释"模拟盘单所运行"不是理由，这是在往事件里填一个已知为假的值。

3. 数量向下取整可能得 0，全链路无人拦截；min_order_size 只写不读

ExchangeOrder::from_domain（client.rs:129）用 round_size_down 向下取整，小于一个 size_step 的量直接变 0。OutcomeProcessorActor 不校验就 place_order。

SymbolMeta::min_order_size（symbol_meta.rs:17）四个 client 全都认真填了，全仓库零处读取 —— 这个字段存在的意义正是拦这一刀。同样零调用的还有 SymbolMeta::is_valid()。

表现是：发出 qty=0 的单 → 被拒 → send_order_error → OrderUpdate(Error) → pending 被移除 → 下一个事件再来一遍，静默重试环。

4. IBKR 的 sor 推送会在 SymbolState 里注册出幽灵挂单

src/exchange/ibkr/actor/public_ws.rs（handle_order_update）构造的 OrderUpdate 是 price: 0.0, quantity: 0.0, reduce_only: false（写死，注释说 sor 不含这些信息）。

state.rs:298-324 的分支：收到 Pending/PartiallyFilled 且本地无该 client_order_id 时，用 update 的字段重建一张 PendingOrder。对 IBKR 就是 price=0、qty=0 的假单。

触发路径真实存在：Created 超时被清理后（state.rs:74）晚到的 Submitted 推送、重连、或带 order_ref 的手工单。之后 has_pending_orders() 恒真，策略被冻住直到终态到达。

5. OKX / Hyperliquid 的 fetch_account_info 返回编造的零值

okx/client.rs:635-643 返回 notional: 0.0；hyperliquid/client.rs:563-570 返回 equity: 0.0, notional: 0.0，注释写"实际使用中不会调用此方法"。

对照同一个 trait 里的 fetch_positions —— 文档明确要求"每个交易所都必须显式表态"，OKX/HL 返回 Ok(vec![]) 并注明数据从 WS 来，那是诚实的空。而 fetch_account_info 返回的是假数。哪天风控或 supervisor 调了它，account_leverage = notional/equity 直接失效（第 9 项里 spread_arb 的账户杠杆闸门就吃这个数）。要么 Err，要么这个方法就不该在 trait 上。

6. fetch_positions 的两个 TODO 是真实的启动期竞态

okx/client.rs:647、hyperliquid/client.rs:574。manager.rs:239-242 把"初始持仓必须在 executor 注册之后、市场订阅之前推"讲得很透，但这个保证只对 REST 直查的所（Binance/IBKR）成立。OKX/HL 靠私有 WS snapshot，早到就被 BestEffort 丢掉 —— "开仓信号在持仓未对齐时发出"的窗口客观存在。这不该以 TODO 形式留在代码里。

---
二、飞线与抽象破口

7. Strategy trait 双分发，作者自己标了"最小改动"

src/engine/strategy_runner.rs:117-127

// (最小改动：仅这两类拆出，其余保持 enum-match 风格的 on_event。)
let raw = match &event.data {
    BorrowFee(bf)    => self.strategy.on_borrow_fee(bf, &self.state),
    ExchangeRate(er) => self.strategy.on_exchange_rate(er, &self.state),
    _                => self.strategy.on_event(event, &self.state),
};

同一条事件流上两套分发风格。行为陷阱：策略在 on_event 里 match BorrowFee 永远收不到，编译器不报错。state_manager.rs:209-211 为这两类另开了空分支，注释同样写"最小改动"。

而这两类事件在本仓库零消费方 —— 没有任何策略实现这两个方法。为一个外部消费者在核心 trait 上开了第二套分发，并在三个文件留了特判。要么统一走 on_event，要么全部改成 typed 回调，不能一半一半。

8. 事件路由知识存了两份，靠手工同步

- IncomeEvent::routing()（event.rs:113，由 symbol() + exchange() 两个 match 拼出）
- IncomeProcessorActor::event_routing()（income_processor.rs:58-130，第三个 match）

前者只被 StrategyRunner::accepts 用，而 accepts 只有回测调（backtest/engine.rs:282）；实盘走后者。新增一个事件变体要改三处 match，漏一处的表现是"回测收得到、实盘收不到"（或反过来），编译照过。event_routing 完全可以直接用 event.routing()。

9. ManagerActor 对交易所是硬编码四段式，ExchangeClient 抽象没用于装配

manager.rs 里 374-408（建 client）、528-612（spawn actor）、614-644（tokio::join! 四路等待）、646-662（插 map）。加第五个所要改 4 处 + ManagerActorArgs 加字段 + 每个 bin 的 ExchangesConfig 加字段。ManagerActorArgs 是四个具名字段而不是 HashMap<Exchange, _> —— 开放封闭的反面。

IBKR 还破了 ExchangeAccess<C> 这个统一形状：不带 quote/dex，凭证里塞了 symbols: Vec<String>（其他三所的 symbol 由策略订阅推导），外加一个平行的 ibkr_snapshot 字段。同一件事两种形状。

10. GetIbkrClient 是明确的开洞，且零调用方

manager.rs:86-88 / 856-869：从类型擦除体系里专门为 IBKR 留一个具体类型出口，注释直说"供策略调用非 ExchangeClient trait 的方法"。全仓库无人调用。PublishIncome（manager.rs:839）同理，零调用方。

这两个洞 + 第 7 项的 BorrowFee/ExchangeRate 通道，是同一个外部消费者在核心里凿的三个口子，本仓库一个都没用上，却全都在核心的 match 里留了分支。

---
三、其它

11. SymbolState 里的 Position 是个半真半假的混合体（state.rs:251-365）。快照只初始化一次，Fill 只累加 size，entry_price 永远停在启动快照、unrealized_pnl 永远是启动值。"持仓靠 Fill 增量维护"的设计和注释都对，但同一个 struct 的另外两个字段就此腐烂。Ledger::apply_fill（ledger.rs:47-74）已经有完整的均价维护逻辑，可以直接复用；否则这两个字段应该从这条路径的 Position 上拿掉。
12. RemoveStrategies 的命中判据会误杀多 symbol 实例（manager.rs:900-901）。targets.iter().any(|s| reg.symbols.contains(s)) —— 只要覆盖的 symbol 里命中一个就整个 kill，而 flatten 只平 targets 里的那些。对多 symbol 策略（DipMaker 就是）就是"降级一个 symbol，连带砍掉另外九个，且这九个的仓位无人看管" —— 恰好是这段代码注释里说要避免的事。
13. 零碎：
- manager.rs:209 日志文案串了（注册 executor 失败，打的是 "Failed to forward event to executor"）。
- impl HyperliquidCredentials {} 空 impl 块。
- IbkrPublicWsActor 名为 public，实际同时处理 sor（订单）和 str（成交）两个私有 topic；其他三所都是 public/private 分开。功能上没错（is_account_private 按事件类型判定而非来源），但名字和职责对不上。
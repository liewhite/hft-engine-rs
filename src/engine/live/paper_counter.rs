//! PaperCounter —— 本地柜台（模拟盘）。
//!
//! 模拟盘与实盘的差异**只在 outcome 事件的流向**：实盘由
//! [`crate::engine::OutcomeProcessor`] 把订单发往交易所，模拟盘改由本组件在本地成交。
//! 二者是出向总线的两种订阅者，各按账户归属认领自己的信号，不在运行期用 `if` 分发。
//!
//! 行情仍是**真实实盘行情**（照常订阅公共 WS），因此策略、行情链路、特征计算与实盘完全同一
//! 份代码；唯一被替换的是柜台。
//!
//! # 撮合语义
//!
//! 复用 [`SimState`]（与回测同一份状态机，保证两边口径一致）：
//! - **只以真实成交判定撮合**：`trade` 价格严格越过挂单价才成交（[`crate::sim::matcher`]）。
//!   盘口只经 [`SimState::observe_bbo`] 更新，不参与撮合 —— 盘口触到你的价位只说明队列
//!   **前面**的人在成交，据此判定成交会系统性高估。
//! - **部分成交**：一笔 print 按其成交量消耗挂单（时间优先），吃不完的留簿等后续成交。
//! - **下单延迟**：订单/撤单到达柜台后需等待 `order_delay_ms` 才开始参与撮合，模拟到交易所
//!   的单程链路时延。延迟期内到达的成交不会打到该单上。
//!
//! # 为何不施加"交易所 -> 策略"方向的延迟
//!
//! [`SimConfig::exchange_to_strategy_delay_ms`] 在回测里用于模拟行情/回报的入向时延；模拟盘
//! 的行情来自真实 WS，本身已带真实网络时延，再叠加会重复计算。故此处只用出向延迟。
//!
//! # 回报直接以 AccountEvent（带 Paper 标签）发布到共享的 AccountBus
//!
//! 与实盘适配层发布**同一个类型**，只是 account 字段不同 —— 账户隔离由字段值保证。
//! SimState 只产回报不回显行情（历史上回显行情曾迫使本组件维护"哪些事件可外发"的
//! 白名单来防自订阅回环，类型拆分后该机制整体消失）。
//!
//! # 账户：按 (账户, 交易所) 分柜台
//!
//! 一份 [`SimState`] 只代表**一个账户在一个交易所**的柜台（见其类型文档）。本组件按
//! (AccountId, Exchange) 分实例：行情按所路由，持仓、挂单簿与净值天然按所隔离 —— 跨所
//! 策略绑一个模拟账户时，A 所挂单不会被 B 所的成交撮合。每个 (账户, 所) 各以
//! `initial_balance_usdt` 起步（等价"每个所各入金一份"，与实盘各所独立账户一致）。
//! 净值在收到 Clock 时按各自的所发布 AccountInfo。

use crate::domain::{
    now_ms, AccountId, Exchange, Order, OrderId, Symbol, SymbolMeta, Timestamp, BBO,
};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, MarketBus, MarketData, MarketEvent,
};
use crate::sim::{SimConfig, SimState};
use crate::strategy::OutcomeEvent;
use crate::tasks::{Group, Inbox, Scope, Task};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use super::{to_paper_outlet, AccountOutcome, OutcomeBus};

/// PaperCounter 构造参数
pub struct PaperCounterArgs {
    /// 撮合与账本配置（每个模拟账户各自按此初始化）
    pub config: SimConfig,
    /// Symbol 元数据：透传给各柜台的 [`SimState`]（市场规则校验在那里统一执行）
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 订阅：全量行情（盘口/成交/标记价撮合与估值，Clock 触发净值发布）
    pub market_bus: MarketBus,
    /// 订阅：出向总线上归本地柜台的信号（过滤 [`to_paper_outlet`]）
    pub outcome_bus: OutcomeBus,
    /// 发布：柜台回报以 Paper 标签发到账户总线（与实盘适配层同一条总线）
    pub account_bus: AccountBus,
}

/// 延迟到期后应用下单（交易所身份取 `order.exchange`，不另存会分叉的副本）
struct ApplyPlace {
    account: AccountId,
    order: Order,
    order_id: OrderId,
}

/// 延迟到期后应用撤单
struct ApplyCancel {
    account: AccountId,
    exchange: Exchange,
    symbol: Symbol,
    client_order_id: String,
}

/// 在延迟队列里等到期的消息（见 [`PaperCounter::schedule`]）
enum Delayed {
    Place(ApplyPlace),
    Cancel(ApplyCancel),
}

/// 收件箱消息：两条总线的订阅都落到这里
enum Msg {
    Market(MarketEvent),
    Outcome(AccountOutcome),
}

/// 本地柜台
pub struct PaperCounter {
    inbox: Inbox<Msg>,
    /// **每个 (模拟账户, 交易所)** 一份虚拟柜台状态（账本 + 挂单簿 + 该所视角的最新行情）。
    ///
    /// 按账户分是为了各模拟账户的盈亏彼此独立；按所分是 SimState 的边界要求
    /// （一份实例只代表一个所，见其类型文档）。柜台在首次收到其订单时惰性创建。
    states: HashMap<(AccountId, Exchange), SimState>,
    /// 共享报价缓存：行情没有账户归属，新柜台创建时用**本所**的报价播种，避免首单因
    /// "还没见过盘口"而无法判定可成交性
    last_quotes: HashMap<(Exchange, Symbol), BBO>,
    /// 共享成交价缓存，与 [`Self::last_quotes`] 同一用途的成交流对偶。
    ///
    /// 两者都要缓存，因为可成交性判定是"盘口对手价 > 最新成交价"二选一：只播种盘口的话，
    /// 纯成交流行情（策略只订 Trades）下新柜台的首单会因两者皆无而被错判。
    last_trades: HashMap<(Exchange, Symbol), crate::domain::Price>,
    account_bus: AccountBus,
    config: SimConfig,
    /// 透传给各柜台 SimState（市场规则校验在状态机内统一执行）
    symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// 本地订单号发号器（模拟交易所返回的 order_id）
    order_id_gen: u64,
    /// 延迟队列：(到期时刻, 消息)，FIFO（顺序保证见 [`Self::schedule`]）
    delayed: VecDeque<(Instant, Delayed)>,
}

impl PaperCounter {
    /// 在 `group` 下起本地柜台，等它订阅完成。订阅与发布见 [`PaperCounterArgs`] 各字段。
    pub async fn spawn(group: &Group, args: PaperCounterArgs) -> anyhow::Result<()> {
        group
            .spawn("PaperCounter", move |scope: Scope| async move {
                let (_addr, mut inbox) = scope.inbox::<Msg>();
                inbox.subscribe(&args.outcome_bus, to_paper_outlet, Msg::Outcome);
                inbox.subscribe(&args.market_bus, |_| true, Msg::Market);
                tracing::warn!(
                    initial_balance = args.config.initial_balance_usdt,
                    maker_fee_rate = args.config.maker_fee_rate,
                    taker_fee_rate = args.config.taker_fee_rate,
                    order_delay_ms = args.config.order_to_exchange_delay_ms,
                    "PaperCounter started — PAPER TRADING, orders are matched locally and never sent to any exchange"
                );
                Ok(Self {
                    inbox,
                    states: HashMap::new(),
                    last_quotes: HashMap::new(),
                    last_trades: HashMap::new(),
                    account_bus: args.account_bus,
                    config: args.config,
                    symbol_metas: args.symbol_metas,
                    order_id_gen: 0,
                    delayed: VecDeque::new(),
                })
            })
            .await?;
        Ok(())
    }

    fn next_order_id(&mut self) -> OrderId {
        self.order_id_gen += 1;
        format!("paper-{}", self.order_id_gen)
    }

    /// 发布柜台产生的回报（SimState 只产 OrderUpdate/Fill，天生无需过滤）。
    fn publish_reports(&self, account: &AccountId, reports: Vec<(Timestamp, AccountData)>) {
        // `local_ts` 必须是**本地时刻**，不能沿用撮合时间戳：行情触发的成交，其 ts 取自
        // 撮合它的那条行情（`bbo.timestamp` / `trade.timestamp`），而那是**交易所打点**。
        // 拿交易所时钟当本地接收时刻有两处实害：executor 的 `check_pipeline_lag` 会把
        // 对端时钟偏移误诊成"本实例跟不上事件流"，`add_pending_order` 的 created_at 与
        // Clock 里的 `now_ms()` 混算会让订单超时判定偏移。
        // `exchange_ts` 保留撮合时刻 —— 那一栏问的正是"交易所侧什么时候发生的"。
        let local_ts = now_ms();
        for (ts, data) in reports {
            self.account_bus.publish(AccountEvent {
                account: account.clone(),
                exchange_ts: ts,
                local_ts,
                data,
            });
        }
    }

    /// 取 (账户, 交易所) 的柜台状态，不存在则创建并用**本所**的共享报价播种。
    fn state_mut(&mut self, account: &AccountId, exchange: Exchange) -> &mut SimState {
        let key = (account.clone(), exchange);
        if !self.states.contains_key(&key) {
            let mut state = SimState::empty(
                exchange,
                self.config.initial_balance_usdt,
                self.config.maker_fee_rate,
                self.config.taker_fee_rate,
                self.symbol_metas.clone(),
            );
            for ((ex, _), bbo) in &self.last_quotes {
                if *ex == exchange {
                    state.observe_bbo(bbo);
                }
            }
            for ((ex, symbol), price) in &self.last_trades {
                if *ex == exchange {
                    state.observe_trade(symbol, *price);
                }
            }
            tracing::info!(
                %account,
                %exchange,
                initial_balance = self.config.initial_balance_usdt,
                "[PAPER] Opened paper account"
            );
            self.states.insert(key.clone(), state);
        }
        self.states.get_mut(&key).expect("just inserted")
    }

    /// 逐 (账户, 交易所) 发布净值 AccountInfo（等价实盘的 equity 轮询）与各币持仓价值（等价实盘账本的读数）。
    /// 交易所字段取各柜台自己的所 —— 此前写死 Binance，OKX/HL 模拟盘的策略查自己所的
    /// equity 恒为 None、永不下单。
    fn publish_account_info(&self, ts: Timestamp) {
        for ((account, _), state) in &self.states {
            let equity = state.ledger.equity(|s: &Symbol| state.mark_of(s));
            let notional = state.ledger.notional(|s: &Symbol| state.mark_of(s));
            self.account_bus.publish(AccountEvent {
                account: account.clone(),
                exchange_ts: ts,
                local_ts: ts,
                data: AccountData::Exchange(ExchangeAccountData::AccountInfo {
                    exchange: state.exchange,
                    equity,
                    notional,
                }),
            });
            // 与实盘账本同一读数：各币持仓价值（策略的账户杠杆闸门用它，#759）
            let legs = state.ledger.position_values(|s: &Symbol| state.mark_of(s));
            self.account_bus.publish(AccountEvent {
                account: account.clone(),
                exchange_ts: ts,
                local_ts: ts,
                data: AccountData::Exchange(ExchangeAccountData::PositionValues(
                    crate::messaging::PositionValues {
                        exchange: state.exchange,
                        legs,
                    },
                )),
            });
        }
    }

    /// 延迟 `order_delay_ms` 后再应用到柜台，模拟到交易所的单程时延。
    ///
    /// # 顺序保证：到达柜台的顺序 == 提交顺序
    ///
    /// 走**单条 FIFO 队列 + `run` 里按队首到期时刻等待**，而不是每条消息各 `tokio::spawn`
    /// 一个 sleep —— 后者**不保证顺序**：同一延迟的两个 sleep 落在同一个定时器槽里时，唤醒
    /// 次序由 tokio 内部的槽内链表决定，与提交序无关（更早的版本曾断言"延迟为常量故不会
    /// 乱序"，那是没有依据的）。
    ///
    /// # 它实际防住什么（如实说明，别高估）
    ///
    /// **不是**"撤单抢在下单前到达"：撤单要带交易所订单号，而该号只在下单入簿后的
    /// `Pending` 回报里才有（gamma_scalp 的 `cancel_confirmed` 也显式要求先确认），
    /// 所以撤单在因果上必然晚于它那张单的入簿，这条竞态**不可达**。
    ///
    /// 真正受影响的是**同时提交的多张挂单之间的时间优先**：`SimState::resting` 是
    /// `IndexMap`，插入序即撮合序（时间优先的近似）。乱序入簿会把后提交的单排到前面，
    /// 使部分成交的预算分配偏离真实交易所。影响有界（都是本策略自己的单），但既然
    /// "按到达顺序撮合"是撮合层写明的语义，就该让它真的成立。
    ///
    /// 所有延迟同为常量，故入队序即到期序，按队首逐条等到期即兑现该语义。
    fn schedule(&mut self, delay_ms: u64, msg: Delayed) {
        let due = Instant::now() + Duration::from_millis(delay_ms);
        self.delayed.push_back((due, msg));
    }

    // === 行情入向：只更新盘口，用真实成交撮合 ===

    fn on_market(&mut self, ev: &MarketEvent) {
        match &ev.data {
            MarketData::BBO(bbo) => {
                // 行情没有账户归属：缓存一份供新柜台播种，并同步给**本所**的已开柜台
                self.last_quotes
                    .insert((bbo.exchange, bbo.symbol.clone()), bbo.clone());
                for ((_, ex), state) in self.states.iter_mut() {
                    if *ex == bbo.exchange {
                        state.observe_bbo(bbo);
                    }
                }
            }
            MarketData::MarketTrade(trade) => {
                // 缓存一份供**新柜台**播种参考价（与 BBO 同理，见 last_trades 字段说明）
                self.last_trades
                    .insert((trade.exchange, trade.symbol.clone()), trade.price);
                // 逐柜台撮合（只喂本所的柜台）：各柜台只持有自己 symbol 的挂单，非本
                // symbol 的柜台是空转。该 O(柜台数) 开销是选择 Top-N 而非全部 530 个
                // symbol 的原因之一；若要放到全量，应把"共享行情"从 SimState 里拆出来只存一份。
                self.match_on_exchange(trade.exchange, ev);
            }
            MarketData::MarkPrice(mp) => self.match_on_exchange(mp.exchange, ev),
            MarketData::Clock => self.publish_account_info(ev.local_ts),
            _ => {}
        }
    }

    /// 把一条行情喂给 `exchange` 上的全部柜台，发布它们产出的回报
    fn match_on_exchange(&mut self, exchange: Exchange, ev: &MarketEvent) {
        let keys: Vec<(AccountId, Exchange)> = self
            .states
            .keys()
            .filter(|(_, ex)| *ex == exchange)
            .cloned()
            .collect();
        for key in keys {
            let reports = self
                .states
                .get_mut(&key)
                .expect("state exists")
                .on_market(ev);
            self.publish_reports(&key.0, reports);
        }
    }

    // === 订单出向：改为落到本地柜台 ===

    fn on_outcome(&mut self, tagged: AccountOutcome) {
        // 归属由订阅过滤器保证（见 [`Self::spawn`]，判据只有 `outlet_for` 一处）
        let account = tagged.account;
        let delay = self.config.order_to_exchange_delay_ms;
        match tagged.event {
            // 自定义事件不进柜台：它已随出向总线到达外部订阅者，柜台只撮合下单/撤单
            OutcomeEvent::Emit(_) => {}
            OutcomeEvent::PlaceOrders { orders, comment } => {
                for order in orders {
                    // 市场规则校验（下界 + 取整）不在此处：柜台状态机 SimState 在订单
                    // 到达时统一执行（与实盘出口同一规则出处 checked_exchange_qty），
                    // 校验失败以 Rejected 终态回报 —— 回测与模拟盘因此天然同规。
                    let order_id = self.next_order_id();
                    tracing::info!(
                        %account,
                        exchange = %order.exchange,
                        symbol = %order.symbol,
                        side = %order.side,
                        order_type = ?order.order_type,
                        quantity = %order.quantity,
                        client_order_id = %order.client_order_id,
                        order_id = %order_id,
                        signal = %comment,
                        delay_ms = delay,
                        "[PAPER] Order accepted by local counter"
                    );
                    self.schedule(
                        delay,
                        Delayed::Place(ApplyPlace {
                            account: account.clone(),
                            order,
                            order_id,
                        }),
                    );
                }
            }
            OutcomeEvent::CancelOrder {
                exchange,
                symbol,
                client_order_id,
            } => {
                tracing::info!(
                    %account, %exchange, %symbol, %client_order_id, delay_ms = delay,
                    "[PAPER] Cancel accepted by local counter"
                );
                self.schedule(
                    delay,
                    Delayed::Cancel(ApplyCancel {
                        account,
                        exchange,
                        symbol,
                        client_order_id,
                    }),
                );
            }
        }
    }

    // === 延迟到期：订单真正进入挂单簿 / 离开挂单簿 ===

    fn apply(&mut self, msg: Delayed) {
        let now = now_ms();
        match msg {
            Delayed::Place(p) => {
                let reports = self
                    .state_mut(&p.account, p.order.exchange)
                    .on_order_arrived(now, &p.order, &p.order_id);
                self.publish_reports(&p.account, reports);
            }
            Delayed::Cancel(c) => {
                // 不在簿（已成交 / 已撤）时柜台回报撤单失败，与实盘同形
                let reports = self.state_mut(&c.account, c.exchange).on_cancel_arrived(
                    now,
                    &c.symbol,
                    &c.client_order_id,
                );
                self.publish_reports(&c.account, reports);
            }
        }
    }
}

impl Task for PaperCounter {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        loop {
            let next_due = self.delayed.front().map(|(due, _)| *due);
            tokio::select! {
                // 收件箱先于已到期的延迟单：到期时仍排在箱里的行情多半是延迟期内到的，先应用
                // 延迟单会让它们打到该单上，违背模块文档「延迟期内到达的成交不会打到该单上」。
                // 代价：柜台忙到收件箱排不空时延迟单会晚于到期入簿，这段时间的成交也打不到它 ——
                // 偏向少成交，与撮合层"不高估成交"的取向一致。
                biased;
                msg = self.inbox.recv() => match msg {
                    Some(Msg::Market(ev)) => self.on_market(&ev),
                    Some(Msg::Outcome(tagged)) => self.on_outcome(tagged),
                    // 已被要求停止，收件箱已排空
                    None => break,
                },
                _ = tokio::time::sleep_until(next_due.unwrap_or_else(Instant::now)), if next_due.is_some() => {
                    let (_, msg) = self.delayed.pop_front().expect("队首刚看过");
                    self.apply(msg);
                }
            }
        }
        // 还没到期的模拟单/撤单随停机丢弃：模拟账户不持久化，下次启动从零起步
        if !self.delayed.is_empty() {
            tracing::warn!(
                dropped = self.delayed.len(),
                "PaperCounter 停机时延迟队列里仍有未到期的模拟下单/撤单，丢弃"
            );
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        tracing::info!("PaperCounter stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MarketTrade, OrderStatus, OrderType, Side, TimeInForce, BBO};
    use crate::engine::live::test_support::collect;
    use crate::messaging::SymbolAccountData;
    use crate::tasks::Root;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::sync::Mutex;

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn account() -> AccountId {
        AccountId::Paper(SYM.to_string())
    }

    /// 测试元数据：步长/最小量取得足够小，既有测试的数量不触发下界校验
    fn test_metas() -> Arc<HashMap<(Exchange, Symbol), SymbolMeta>> {
        use crate::domain::PriceStep;
        Arc::new(
            [Exchange::Binance, Exchange::OKX]
                .into_iter()
                .map(|exchange| {
                    (
                        (exchange, SYM.to_string()),
                        SymbolMeta {
                            exchange,
                            symbol: SYM.to_string(),
                            price_rule: Arc::new(PriceStep::new(dec!(0.001))),
                            size_step: dec!(0.001),
                            min_order_size: dec!(0.001),
                            min_notional: 0.0,
                            contract_size: dec!(1.0),
                        },
                    )
                })
                .collect(),
        )
    }

    fn config(order_delay_ms: u64) -> SimConfig {
        SimConfig {
            initial_balance_usdt: 10_000.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
            order_to_exchange_delay_ms: order_delay_ms,
            exchange_to_strategy_delay_ms: 0,
        }
    }

    /// 柜台挂在测试树上，经真实的总线订阅收发（订阅过滤也一并测到）
    struct Harness {
        root: Root,
        market_bus: MarketBus,
        outcome_bus: OutcomeBus,
        /// 账户总线上的柜台回报，供断言
        events: Arc<Mutex<Vec<AccountEvent>>>,
    }

    impl Harness {
        async fn new(order_delay_ms: u64) -> Self {
            Self::with_config(config(order_delay_ms)).await
        }

        async fn with_config(config: SimConfig) -> Self {
            let root = Root::new("t");
            let group = root.group("g");
            let market_bus = MarketBus::new("market");
            let outcome_bus = OutcomeBus::new("outcome");
            let account_bus = AccountBus::new("account");
            let events = collect(&group, &account_bus).await;
            PaperCounter::spawn(
                &group,
                PaperCounterArgs {
                    config,
                    symbol_metas: test_metas(),
                    market_bus: market_bus.clone(),
                    outcome_bus: outcome_bus.clone(),
                    account_bus,
                },
            )
            .await
            .expect("PaperCounter 起不来");
            Self {
                root,
                market_bus,
                outcome_bus,
                events,
            }
        }

        fn market(&self, ev: MarketEvent) {
            self.market_bus.publish(ev);
        }

        fn outcome(&self, o: AccountOutcome) {
            self.outcome_bus.publish(o);
        }

        async fn finish(self) {
            self.root.shutdown().await.expect("正常停机");
        }

        /// 送一条 BBO（只更新盘口，不应撮合）
        async fn bbo(&self, bid: Decimal, ask: Decimal) {
            let ev = MarketEvent {
                exchange_ts: 1,
                local_ts: 1,
                data: MarketData::BBO(BBO {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    bid_price: bid,
                    bid_qty: dec!(1.0),
                    ask_price: ask,
                    ask_qty: dec!(1.0),
                    timestamp: 1,
                }),
            };
            self.market(ev);
        }

        /// 送一条真实成交
        async fn trade(&self, price: Decimal) {
            let ev = MarketEvent {
                exchange_ts: 2,
                local_ts: 2,
                data: MarketData::MarketTrade(MarketTrade {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    price,
                    qty: dec!(1.0),
                    is_buyer_maker: false,
                    timestamp: 2,
                }),
            };
            self.market(ev);
        }

        async fn place(&self, side: Side, price: Decimal, qty: Decimal) {
            self.place_tif(side, price, qty, TimeInForce::PostOnly)
                .await;
        }

        async fn place_tif(&self, side: Side, price: Decimal, qty: Decimal, tif: TimeInForce) {
            let order = Order {
                exchange: EX,
                symbol: SYM.to_string(),
                side,
                order_type: OrderType::Limit { price, tif },
                quantity: qty,
                reduce_only: false,
                client_order_id: "c1".to_string(),
            };
            self.outcome(AccountOutcome {
                account: account(),
                event: OutcomeEvent::PlaceOrders {
                    orders: vec![order],
                    comment: "t".to_string(),
                },
            });
        }

        /// 等待柜台把已排队的消息处理完（总线投递与延迟队列都是异步的，需给出时间）
        async fn settle(&self, ms: u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }

        fn statuses(&self) -> Vec<OrderStatus> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match &e.data {
                    AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => {
                        Some(u.status.clone())
                    }
                    _ => None,
                })
                .collect()
        }

        /// 柜台回的「下单请求被拒」（与实盘出口同样的结论，#812）
        fn rejected(&self) -> bool {
            self.events.lock().unwrap().iter().any(|e| {
                matches!(
                    &e.data,
                    AccountData::Symbol(SymbolAccountData::OrderRequest(r))
                        if matches!(r.outcome, crate::messaging::RequestOutcome::PlaceRejected { .. })
                )
            })
        }

        fn fills(&self) -> Vec<(Decimal, Decimal)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match &e.data {
                    AccountData::Symbol(SymbolAccountData::Fill(f)) => Some((f.price, f.size)),
                    _ => None,
                })
                .collect()
        }
    }

    /// **跨所隔离**：A 所的挂单绝不被 B 所同 symbol 的成交撮合（柜台按 (账户, 所) 分 SimState）。
    /// 此前 SimState 无交易所维度、挂单只按 symbol 过滤，跨所策略绑一个模拟账户即串账。
    #[tokio::test]
    async fn cross_exchange_trade_never_fills_other_exchanges_order() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        // Binance 上挂买单 @99
        h.place(Side::Long, dec!(99.0), dec!(0.5)).await;
        h.settle(50).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending]);
        // OKX 上同 symbol 的成交穿过挂单价 —— 不得撮合 Binance 的挂单
        let okx_trade = MarketEvent {
            exchange_ts: 2,
            local_ts: 2,
            data: MarketData::MarketTrade(MarketTrade {
                exchange: Exchange::OKX,
                symbol: SYM.to_string(),
                price: dec!(98.0),
                qty: dec!(1.0),
                is_buyer_maker: false,
                timestamp: 2,
            }),
        };
        h.market(okx_trade);
        h.settle(50).await;
        assert!(
            h.fills().is_empty(),
            "OKX 的成交撮合了 Binance 的挂单 —— 跨所串账"
        );
        // Binance 自己的成交才撮合
        h.trade(dec!(98.0)).await;
        h.settle(50).await;
        assert_eq!(h.fills(), vec![(dec!(99.0), dec!(0.5))]);
        h.finish().await;
    }

    /// 柜台净值按各自的所发布（此前写死 Binance，OKX/HL 模拟盘的策略查自己所的 equity
    /// 恒为 None、永不下单）
    #[tokio::test]
    async fn account_info_carries_the_counters_own_exchange() {
        let h = Harness::new(0).await;
        // 在 OKX 上下一单，惰性开出 (账户, OKX) 柜台
        let order = Order {
            exchange: Exchange::OKX,
            symbol: SYM.to_string(),
            side: Side::Long,
            order_type: OrderType::Limit {
                price: dec!(99.0),
                tif: TimeInForce::PostOnly,
            },
            quantity: dec!(0.5),
            reduce_only: false,
            client_order_id: "c-okx".to_string(),
        };
        h.outcome(AccountOutcome {
            account: account(),
            event: OutcomeEvent::PlaceOrders {
                orders: vec![order],
                comment: "t".to_string(),
            },
        });
        h.settle(50).await;
        // Clock 触发净值发布
        h.market(MarketEvent {
            exchange_ts: 3,
            local_ts: 3,
            data: MarketData::Clock,
        });
        h.settle(50).await;
        let exchanges: Vec<Exchange> = h
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.data {
                AccountData::Exchange(ExchangeAccountData::AccountInfo { exchange, .. }) => {
                    Some(*exchange)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            exchanges,
            vec![Exchange::OKX],
            "净值必须带柜台自己的所，不是写死的 Binance"
        );
        h.finish().await;
    }

    /// 成交只由**真实成交越过挂单价**判定；盘口越价不算
    #[tokio::test]
    async fn fill_is_decided_by_trade_crossing_not_by_quote() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(99.0), dec!(0.5)).await;
        h.settle(50).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending]);

        // 盘口整体下移、ask 已跌破挂单价 —— 但不据此成交
        h.bbo(dec!(98.0), dec!(98.5)).await;
        h.settle(30).await;
        assert!(h.fills().is_empty(), "盘口越价不应成交");

        // 真实成交价严格跌破挂单价 -> 成交，且成交价取挂单价（maker）
        h.trade(dec!(98.9)).await;
        h.settle(30).await;
        assert_eq!(h.fills(), vec![(dec!(99.0), dec!(0.5))]);
        assert!(h.statuses().contains(&OrderStatus::Filled));
        h.finish().await;
    }

    /// **与实盘同规则的下界校验**：实盘必拒的单（不足一个 size_step）模拟盘也拒，
    /// 以「下单请求被拒」回报（与实盘出口同形，清策略侧 pending），绝不入簿成交 —— 否则模拟盘会成交
    /// 实盘发不出去的单，仿真失真。校验由柜台状态机 SimState 统一执行（回测同一份）。
    #[tokio::test]
    async fn undersized_order_is_rejected_like_live() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        // 0.0004 < size_step 0.001：实盘出口会拒，柜台同样拒
        h.place(Side::Long, dec!(99.0), dec!(0.0004)).await;
        h.settle(50).await;
        assert!(h.rejected(), "校验失败必须回报请求被拒: {:?}", h.statuses());
        // 即使成交穿价，也不该有成交（单没入簿）
        h.trade(dec!(98.0)).await;
        h.settle(30).await;
        assert!(h.fills().is_empty(), "未通过校验的单不该成交");
        h.finish().await;
    }

    /// **部分成交**：一笔 print 只按其数量消耗挂单（此前 qty 被忽略，任意小的 print
    /// 可以吃掉全部挂单量，成交量被系统性高估），剩余量留在簿里等后续成交
    #[tokio::test]
    async fn trade_qty_budget_yields_partial_fills() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(99.0), dec!(7.5)).await;
        h.settle(50).await;
        // 成交量 1.0 < 挂单量 7.5：只成交 1.0，剩余 6.5 留簿
        h.trade(dec!(98.0)).await;
        h.settle(30).await;
        assert_eq!(h.fills(), vec![(dec!(99.0), dec!(1.0))]);
        assert!(
            h.statuses().contains(&OrderStatus::PartiallyFilled),
            "部分成交必须以 PartiallyFilled 回报: {:?}",
            h.statuses()
        );
        // 后续 print 继续消耗剩余量
        h.trade(dec!(98.0)).await;
        h.settle(30).await;
        assert_eq!(
            h.fills(),
            vec![(dec!(99.0), dec!(1.0)), (dec!(99.0), dec!(1.0))]
        );
        h.finish().await;
    }

    /// 延迟期内到达的成交打不到该单上
    #[tokio::test]
    async fn order_does_not_match_before_delay_elapses() {
        let h = Harness::new(300).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(99.0), dec!(0.5)).await;

        // 延迟未到：单子还没进簿，越价成交也不该成交
        h.settle(60).await;
        h.trade(dec!(98.0)).await;
        h.settle(30).await;
        assert!(h.statuses().is_empty(), "延迟未到不应有任何回报");
        assert!(h.fills().is_empty(), "延迟未到不应成交");

        // 延迟到期后入簿
        h.settle(300).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending]);
        assert!(h.fills().is_empty(), "入簿后还需新的越价成交才成交");

        h.trade(dec!(98.0)).await;
        h.settle(30).await;
        assert_eq!(h.fills(), vec![(dec!(99.0), dec!(0.5))]);
        h.finish().await;
    }

    /// 延迟期内到达、到期那一刻还排在收件箱里的成交，同样打不到该单（收件箱先于到期的延迟单）
    #[tokio::test(start_paused = true)]
    async fn trade_still_queued_when_delay_elapses_does_not_hit_the_order() {
        let h = Harness::new(100).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(99.0), dec!(0.5)).await;
        h.settle(99).await; // t=99：单子已排队，100 到期
                            // 柜台积压：一批越价成交进箱时柜台还没轮到运行。时钟随后一步跨过到期时刻，柜台
                            // 处理积压途中会让出执行权（tokio 协作预算），时间驱动随即把到期定时器置为就绪 ——
                            // 此时箱里仍有延迟期内到的成交，正是两条分支同时就绪的时刻
        for _ in 0..1000 {
            h.trade(dec!(98.0)).await;
        }
        tokio::time::advance(Duration::from_millis(5)).await;
        h.settle(10).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending]);
        assert!(h.fills().is_empty(), "延迟期内到的成交打到了单上");
        h.finish().await;
    }

    /// 延迟到期时若已越过实时盘口 -> 判定为 taker 全量成交（成交价取对手价）
    #[tokio::test]
    async fn order_crossing_live_quote_on_arrival_fills_as_taker() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        // 买价 101 高于卖价 100.1，到达即可成交
        h.place_tif(Side::Long, dec!(101.0), dec!(0.5), TimeInForce::GTC)
            .await;
        h.settle(50).await;
        // 成交价取对手价 100.1（taker），不是挂单价 101
        assert_eq!(h.fills(), vec![(dec!(100.1), dec!(0.5))]);
        assert!(h.statuses().contains(&OrderStatus::Filled));
        h.finish().await;
    }

    /// **关键语义**：可成交性在**延迟到期那一刻**按当时的实时盘口判定。
    /// 下单时不可成交，延迟期内盘口涨上来越过挂单价 -> 到达时即 taker 成交。
    #[tokio::test]
    async fn marketability_is_judged_when_delay_elapses_not_at_submit() {
        let h = Harness::new(200).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        // 提交时 100.05 低于卖价 100.1，不可成交
        h.place_tif(Side::Long, dec!(100.05), dec!(0.5), TimeInForce::GTC)
            .await;

        // 延迟期内盘口下移，卖价跌到 100.0 <= 100.05 -> 到达时变为可成交
        h.settle(60).await;
        h.bbo(dec!(99.9), dec!(100.0)).await;
        assert!(h.statuses().is_empty(), "延迟未到不应有回报");

        h.settle(250).await;
        assert_eq!(
            h.fills(),
            vec![(dec!(100.0), dec!(0.5))],
            "应按到达时的实时盘口以 taker 成交于对手价 dec!(100.0)"
        );
        h.finish().await;
    }

    /// 反向：提交时可成交，但延迟期内盘口走开 -> 到达时改为挂单（不成交）
    #[tokio::test]
    async fn order_rests_if_quote_moves_away_during_delay() {
        let h = Harness::new(200).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place_tif(Side::Long, dec!(100.2), dec!(0.5), TimeInForce::GTC)
            .await; // 提交时越过卖价

        h.settle(60).await;
        h.bbo(dec!(100.5), dec!(100.6)).await; // 盘口涨走，dec!(100.2) 不再可成交

        h.settle(250).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending], "应改为挂单");
        assert!(h.fills().is_empty());
        h.finish().await;
    }

    /// taker 成交按 taker 费率计费（与 maker 不同）
    #[tokio::test]
    async fn taker_fill_charges_taker_fee() {
        let h = Harness::with_config(SimConfig {
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
            ..config(0)
        })
        .await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place_tif(Side::Long, dec!(101.0), dec!(2.0), TimeInForce::GTC)
            .await;
        h.settle(50).await;

        let fee = h
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match &e.data {
                AccountData::Symbol(SymbolAccountData::Fill(f)) => Some(f.fee),
                _ => None,
            })
            .expect("fill");
        // 100.1 * 2 * 0.0005 = 0.1001（若按 maker 费率会是 0.04004）
        assert!((fee - 0.1001).abs() < 1e-9, "taker 费率应生效，实得 {fee}");
        h.finish().await;
    }

    /// 撤单同样经过延迟；撤掉后不再成交
    #[tokio::test]
    async fn cancel_removes_order_from_book() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(99.0), dec!(0.5)).await;
        h.settle(50).await;

        h.outcome(AccountOutcome {
            account: account(),
            event: OutcomeEvent::CancelOrder {
                exchange: EX,
                symbol: SYM.to_string(),
                client_order_id: "c1".to_string(),
            },
        });
        h.settle(50).await;
        assert!(h.statuses().contains(&OrderStatus::Cancelled));

        h.trade(dec!(98.0)).await;
        h.settle(30).await;
        assert!(h.fills().is_empty(), "已撤单不应成交");
        h.finish().await;
    }

    /// PostOnly 到达时若已可成交 -> 拒单（与真实交易所一致）
    #[tokio::test]
    async fn marketable_post_only_is_rejected() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(100.5), dec!(0.5)).await; // 高于 ask，会吃单
        h.settle(50).await;
        assert!(h.rejected(), "得到 {:?}", h.statuses());
        assert!(h.fills().is_empty());
        h.finish().await;
    }

    /// 行情回显绝不能被再次发布 —— 柜台自己也订阅 income，回显会导致无限循环
    #[tokio::test]
    async fn market_events_are_never_republished() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.trade(dec!(100.0)).await;
        // 标记价同样经 on_market（用于估值），其回显也不得外发
        h.market(MarketEvent {
            exchange_ts: 3,
            local_ts: 3,
            data: MarketData::MarkPrice(crate::domain::MarkPrice {
                exchange: EX,
                symbol: SYM.to_string(),
                price: dec!(100.0),
                timestamp: 3,
            }),
        });
        h.settle(50).await;
        // 柜台只往总线发 AccountEvent，而 AccountData 没有行情变体 —— 「回显行情」在类型上
        // 写不出来，不用再逐条探针；这里守住的是：喂入行情后柜台不卡死、能正常收尾。
        h.finish().await;
    }

    /// Clock 按账户发布净值（策略依赖 equity 才能动作）。
    ///
    /// 账户是惰性创建的：先下单开出账户，Clock 才有账户可报 —— 没有账户就没有净值可言。
    #[tokio::test]
    async fn clock_publishes_account_info_per_account() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.place(Side::Long, dec!(99.0), dec!(0.5)).await;
        h.settle(50).await;
        h.market(MarketEvent {
            exchange_ts: 5,
            local_ts: 5,
            data: MarketData::Clock,
        });
        h.settle(30).await;
        let equity = h.events.lock().unwrap().iter().find_map(|e| match &e.data {
            AccountData::Exchange(ExchangeAccountData::AccountInfo { equity, .. }) => Some(*equity),
            _ => None,
        });
        assert_eq!(equity, Some(10_000.0));
        h.finish().await;
    }

    /// 实盘账户的信号不归本地柜台（订阅过滤 `to_paper_outlet`）：既不入簿也不回报
    #[tokio::test]
    async fn live_signal_is_not_claimed_by_paper_counter() {
        let h = Harness::new(0).await;
        h.bbo(dec!(100.0), dec!(100.1)).await;
        h.outcome(AccountOutcome {
            account: AccountId::Live,
            event: OutcomeEvent::PlaceOrders {
                orders: vec![Order {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    side: Side::Long,
                    order_type: OrderType::Limit {
                        price: dec!(99.0),
                        tif: TimeInForce::GTC,
                    },
                    quantity: dec!(0.5),
                    reduce_only: false,
                    client_order_id: "c-live".to_string(),
                }],
                comment: "t".to_string(),
            },
        });
        h.settle(50).await;
        assert!(
            h.statuses().is_empty(),
            "实盘信号落进了本地柜台: {:?}",
            h.statuses()
        );
        h.finish().await;
    }
}

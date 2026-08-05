//! PaperCounterActor —— 本地柜台（模拟盘）。
//!
//! 模拟盘与实盘的差异**只在 outcome 事件的流向**：实盘由
//! [`crate::engine::OutcomeProcessorActor`] 把订单发往交易所，模拟盘改由本 actor 在本地成交。
//! 二者是 OutcomePubSub 的两种订阅者，在装配时择一，不在运行期用 `if` 分发。
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
//! - **不做部分成交**：越价即全量成交。
//! - **下单延迟**：订单/撤单到达柜台后需等待 `order_delay_ms` 才开始参与撮合，模拟到交易所
//!   的单程链路时延。延迟期内到达的成交不会打到该单上。
//!
//! # 为何不施加"交易所 -> 策略"方向的延迟
//!
//! [`SimConfig::exchange_to_strategy_delay_ms`] 在回测里用于模拟行情/回报的入向时延；模拟盘
//! 的行情来自真实 WS，本身已带真实网络时延，再叠加会重复计算。故此处只用出向延迟。
//!
//! # 账户
//!
//! 每个交易所各自持有一份 [`SimState`]（含 [`crate::sim::Ledger`]），因此持仓与净值按所隔离，
//! 与实盘的 AccountInfo 口径一致；不会把跨所持仓并进一个账本。净值在收到 Clock 时发布。

use crate::domain::{now_ms, AccountId, Exchange, Order, OrderId, Symbol, Timestamp, BBO};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::sim::{SimConfig, SimState};
use crate::strategy::OutcomeEvent;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::time::Duration;

use super::{AccountIncome, AccountOutcome, PaperPubSub};

/// PaperCounterActor 初始化参数
pub struct PaperCounterArgs {
    /// 模拟账户私有事件的发布总线（**不是**共享行情总线）
    pub paper_pubsub: ActorRef<PaperPubSub>,
    /// 撮合与账本配置（每个模拟账户各自按此初始化）
    pub config: SimConfig,
}

/// 延迟到期后应用下单
pub struct ApplyPlace {
    account: AccountId,
    exchange: Exchange,
    order: Order,
    order_id: OrderId,
}

/// 延迟到期后应用撤单
pub struct ApplyCancel {
    account: AccountId,
    exchange: Exchange,
    order_id: OrderId,
}

/// 本地柜台
pub struct PaperCounterActor {
    /// **每个模拟账户**一份虚拟柜台状态（账本 + 挂单簿 + 该账户视角的最新行情）。
    ///
    /// 按账户而非按交易所分账：晋升逻辑需要逐 symbol 的独立盈亏，而每个 symbol 的模拟账户
    /// 本就是一个独立账户（独立起始资金、独立仓位）。账户在首次收到其订单时惰性创建。
    states: HashMap<AccountId, SimState>,
    /// 共享报价缓存：行情没有账户归属，新账户创建时用它播种，避免首单因"还没见过盘口"
    /// 而无法判定可成交性
    last_quotes: HashMap<Symbol, BBO>,
    paper_pubsub: ActorRef<PaperPubSub>,
    config: SimConfig,
    /// 本地订单号发号器（模拟交易所返回的 order_id）
    order_id_gen: u64,
}

impl PaperCounterActor {
    fn next_order_id(&mut self) -> OrderId {
        self.order_id_gen += 1;
        format!("paper-{}", self.order_id_gen)
    }

    /// 发布柜台产生的回报。
    ///
    /// **白名单**：只发 `OrderUpdate` 与 `Fill` —— 柜台的职责就是这两类。
    /// [`SimState::on_market`] 会把入参行情原样放在返回值首位，而柜台自己也订阅 IncomePubSub，
    /// 若把行情回显再发布出去，就会收到自己发的行情、再次回显 —— **无限循环**。用黑名单逐个
    /// 排除行情类型是脆弱的（新增一种行情事件就漏一个），故按白名单放行。
    async fn publish_reports(&self, account: &AccountId, events: Vec<IncomeEvent>) {
        for ev in events {
            if !matches!(
                ev.data,
                ExchangeEventData::OrderUpdate(_) | ExchangeEventData::Fill(_)
            ) {
                continue;
            }
            let tagged = AccountIncome {
                account: account.clone(),
                event: ev,
            };
            if let Err(e) = self.paper_pubsub.tell(Publish(tagged)).send().await {
                tracing::error!(error = %e, "Paper counter failed to publish report");
            }
        }
    }

    /// 取账户的柜台状态，不存在则创建并用共享报价播种。
    fn state_mut(&mut self, account: &AccountId) -> &mut SimState {
        if !self.states.contains_key(account) {
            let mut state = SimState::empty(
                self.config.initial_balance_usdt,
                self.config.maker_fee_rate,
                self.config.taker_fee_rate,
            );
            for bbo in self.last_quotes.values() {
                state.observe_bbo(bbo);
            }
            tracing::info!(
                %account,
                initial_balance = self.config.initial_balance_usdt,
                "[PAPER] Opened paper account"
            );
            self.states.insert(account.clone(), state);
        }
        self.states.get_mut(account).expect("just inserted")
    }

    /// 按 symbol 汇总各所净值并发布 AccountInfo（等价实盘的 equity 轮询）
    async fn publish_account_info(&self, ts: Timestamp, exchange: Exchange) {
        for (account, state) in &self.states {
            let equity = state.ledger.equity(|s: &Symbol| state.mark_of(s));
            let notional = state.ledger.notional(|s: &Symbol| state.mark_of(s));
            let tagged = AccountIncome {
                account: account.clone(),
                event: IncomeEvent {
                    exchange_ts: ts,
                    local_ts: ts,
                    data: ExchangeEventData::AccountInfo {
                        exchange,
                        equity,
                        notional,
                    },
                },
            };
            if let Err(e) = self.paper_pubsub.tell(Publish(tagged)).send().await {
                tracing::error!(error = %e, "Paper counter failed to publish account info");
            }
        }
    }

    /// 延迟 `order_delay_ms` 后把消息投回自己，模拟到交易所的单程时延。
    ///
    /// 延迟为常量，故同一 actor 上的投递顺序与到达顺序一致（不会乱序）。
    fn schedule<M>(actor_ref: &ActorRef<PaperCounterActor>, delay_ms: u64, msg: M)
    where
        PaperCounterActor: Message<M>,
        M: Send + 'static,
    {
        let actor_ref = actor_ref.clone();
        tokio::spawn(async move {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            if let Err(e) = actor_ref.tell(msg).send().await {
                tracing::warn!(error = %e, "Paper counter delayed message dropped (actor stopped)");
            }
        });
    }
}

impl Actor for PaperCounterActor {
    type Args = PaperCounterArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, _r: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::warn!(
            initial_balance = args.config.initial_balance_usdt,
            maker_fee_rate = args.config.maker_fee_rate,
            taker_fee_rate = args.config.taker_fee_rate,
            order_delay_ms = args.config.order_to_exchange_delay_ms,
            "PaperCounterActor started — PAPER TRADING, orders are matched locally and never sent to any exchange"
        );
        Ok(Self {
            states: HashMap::new(),
            last_quotes: HashMap::new(),
            paper_pubsub: args.paper_pubsub,
            config: args.config,
            order_id_gen: 0,
        })
    }

    async fn on_stop(
        &mut self,
        _r: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("PaperCounterActor stopped");
        Ok(())
    }
}

// === 行情入向：只更新盘口，用真实成交撮合 ===

impl Message<IncomeEvent> for PaperCounterActor {
    type Reply = ();

    async fn handle(&mut self, ev: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        match &ev.data {
            ExchangeEventData::BBO(bbo) => {
                // 行情没有账户归属：缓存一份供新账户播种，并同步给所有已开账户
                self.last_quotes.insert(bbo.symbol.clone(), bbo.clone());
                for state in self.states.values_mut() {
                    state.observe_bbo(bbo);
                }
            }
            ExchangeEventData::MarketTrade(trade) => {
                // 逐账户撮合：各账户只持有自己 symbol 的挂单，非本 symbol 的账户是空转。
                // 该 O(账户数) 开销是选择 Top-N 而非全部 530 个 symbol 的原因之一；
                // 若要放到全量，应把"共享行情"从 SimState 里拆出来只存一份。
                let exchange = trade.exchange;
                let accounts: Vec<AccountId> = self.states.keys().cloned().collect();
                for account in accounts {
                    let reports = self
                        .states
                        .get_mut(&account)
                        .expect("account exists")
                        .on_market(exchange, &ev);
                    self.publish_reports(&account, reports).await;
                }
            }
            ExchangeEventData::MarkPrice(mp) => {
                let exchange = mp.exchange;
                let accounts: Vec<AccountId> = self.states.keys().cloned().collect();
                for account in accounts {
                    let reports = self
                        .states
                        .get_mut(&account)
                        .expect("account exists")
                        .on_market(exchange, &ev);
                    self.publish_reports(&account, reports).await;
                }
            }
            ExchangeEventData::Clock => {
                // 净值按账户分别发布；交易所取自任一已知报价所属的所（模拟盘单所运行）
                self.publish_account_info(ev.local_ts, Exchange::Binance).await;
            }
            _ => {}
        }
    }
}

// === 订单出向：改为落到本地柜台 ===

impl Message<AccountOutcome> for PaperCounterActor {
    type Reply = ();

    async fn handle(&mut self, tagged: AccountOutcome, ctx: &mut Context<Self, Self::Reply>) {
        // 只负责模拟账户；实盘账户的订单由 OutcomeProcessorActor 发往交易所
        if !tagged.account.is_paper() {
            return;
        }
        let account = tagged.account;
        let msg = tagged.event;
        let delay = self.config.order_to_exchange_delay_ms;
        match msg {
            OutcomeEvent::PlaceOrders { orders, comment } => {
                for order in orders {
                    let order_id = self.next_order_id();
                    tracing::info!(
                        %account,
                        exchange = %order.exchange,
                        symbol = %order.symbol,
                        side = %order.side,
                        order_type = ?order.order_type,
                        quantity = order.quantity,
                        client_order_id = %order.client_order_id,
                        order_id = %order_id,
                        signal = %comment,
                        delay_ms = delay,
                        "[PAPER] Order accepted by local counter"
                    );
                    Self::schedule(
                        ctx.actor_ref(),
                        delay,
                        ApplyPlace {
                            account: account.clone(),
                            exchange: order.exchange,
                            order,
                            order_id,
                        },
                    );
                }
            }
            OutcomeEvent::CancelOrder {
                exchange,
                symbol,
                order_id,
                // 柜台的 SimState 以 order_id 为键撮合/撤单，回报里自带 client_order_id
                client_order_id: _,
            } => {
                tracing::info!(
                    %account, %exchange, %symbol, %order_id, delay_ms = delay,
                    "[PAPER] Cancel accepted by local counter"
                );
                Self::schedule(
                    ctx.actor_ref(),
                    delay,
                    ApplyCancel {
                        account,
                        exchange,
                        order_id,
                    },
                );
            }
        }
    }
}

// === 延迟到期：订单真正进入挂单簿 / 离开挂单簿 ===

impl Message<ApplyPlace> for PaperCounterActor {
    type Reply = ();

    async fn handle(&mut self, msg: ApplyPlace, _ctx: &mut Context<Self, Self::Reply>) {
        let now = now_ms();
        let reports = self
            .state_mut(&msg.account)
            .on_order_arrived(now, msg.exchange, &msg.order, &msg.order_id);
        self.publish_reports(&msg.account, reports).await;
    }
}

impl Message<ApplyCancel> for PaperCounterActor {
    type Reply = ();

    async fn handle(&mut self, msg: ApplyCancel, _ctx: &mut Context<Self, Self::Reply>) {
        let now = now_ms();
        let reports = self
            .state_mut(&msg.account)
            .on_cancel_arrived(now, msg.exchange, &msg.order_id);
        if reports.is_empty() {
            tracing::info!(
                account = %msg.account,
                exchange = %msg.exchange,
                order_id = %msg.order_id,
                "[PAPER] Cancel had no effect (order already filled or unknown)"
            );
        }
        self.publish_reports(&msg.account, reports).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MarketTrade, OrderStatus, OrderType, Side, TimeInForce, BBO};
    use kameo::actor::Spawn;
    use kameo::mailbox;
    use kameo_actors::pubsub::Subscribe;
    use kameo_actors::DeliveryStrategy;
    use std::sync::{Arc, Mutex};

    const EX: Exchange = Exchange::Binance;
    const SYM: &str = "BTC";

    fn account() -> AccountId {
        AccountId::Paper(SYM.to_string())
    }

    /// 收集柜台回报，供断言
    struct Sink(Arc<Mutex<Vec<IncomeEvent>>>);

    impl Actor for Sink {
        type Args = Arc<Mutex<Vec<IncomeEvent>>>;
        type Error = Infallible;
        async fn on_start(a: Self::Args, _r: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self(a))
        }
    }

    impl Message<AccountIncome> for Sink {
        type Reply = ();
        async fn handle(&mut self, msg: AccountIncome, _c: &mut Context<Self, Self::Reply>) {
            self.0.lock().unwrap().push(msg.event);
        }
    }

    struct Harness {
        counter: ActorRef<PaperCounterActor>,
        events: Arc<Mutex<Vec<IncomeEvent>>>,
    }

    impl Harness {
        async fn new(order_delay_ms: u64) -> Self {
            let pubsub = PaperPubSub::spawn_with_mailbox(
                PaperPubSub::new(DeliveryStrategy::Guaranteed),
                mailbox::unbounded(),
            );
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Sink::spawn_with_mailbox(events.clone(), mailbox::unbounded());
            pubsub.tell(Subscribe(sink)).send().await.unwrap();

            let counter = PaperCounterActor::spawn_with_mailbox(
                PaperCounterArgs {
                    paper_pubsub: pubsub,
                    config: SimConfig {
                        initial_balance_usdt: 10_000.0,
                        maker_fee_rate: 0.0,
                        taker_fee_rate: 0.0,
                        order_to_exchange_delay_ms: order_delay_ms,
                        exchange_to_strategy_delay_ms: 0,
                    },
                },
                mailbox::unbounded(),
            );
            Self { counter, events }
        }

        /// 送一条 BBO（只更新盘口，不应撮合）
        async fn bbo(&self, bid: f64, ask: f64) {
            let ev = IncomeEvent {
                exchange_ts: 1,
                local_ts: 1,
                data: ExchangeEventData::BBO(BBO {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    bid_price: bid,
                    bid_qty: 1.0,
                    ask_price: ask,
                    ask_qty: 1.0,
                    timestamp: 1,
                }),
            };
            self.counter.tell(ev).send().await.unwrap();
        }

        /// 送一条真实成交
        async fn trade(&self, price: f64) {
            let ev = IncomeEvent {
                exchange_ts: 2,
                local_ts: 2,
                data: ExchangeEventData::MarketTrade(MarketTrade {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    price,
                    qty: 1.0,
                    is_buyer_maker: false,
                    timestamp: 2,
                }),
            };
            self.counter.tell(ev).send().await.unwrap();
        }

        async fn place(&self, side: Side, price: f64, qty: f64) {
            self.place_tif(side, price, qty, TimeInForce::PostOnly).await;
        }

        async fn place_tif(&self, side: Side, price: f64, qty: f64, tif: TimeInForce) {
            let order = Order {
                id: String::new(),
                exchange: EX,
                symbol: SYM.to_string(),
                side,
                order_type: OrderType::Limit { price, tif },
                quantity: qty,
                reduce_only: false,
                client_order_id: "c1".to_string(),
            };
            self.counter
                .tell(AccountOutcome {
                    account: account(),
                    event: OutcomeEvent::PlaceOrders {
                        orders: vec![order],
                        comment: "t".to_string(),
                    },
                })
                .send()
                .await
                .unwrap();
        }

        /// 等待柜台把已排队的消息处理完（延迟消息经 tokio::spawn，需给出时间）
        async fn settle(&self, ms: u64) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }

        fn statuses(&self) -> Vec<OrderStatus> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match &e.data {
                    ExchangeEventData::OrderUpdate(u) => Some(u.status.clone()),
                    _ => None,
                })
                .collect()
        }

        fn fills(&self) -> Vec<(f64, f64)> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match &e.data {
                    ExchangeEventData::Fill(f) => Some((f.price, f.size)),
                    _ => None,
                })
                .collect()
        }

        fn order_ids(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match &e.data {
                    ExchangeEventData::OrderUpdate(u) => Some(u.order_id.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    /// 成交只由**真实成交越过挂单价**判定；盘口越价不算
    #[tokio::test]
    async fn fill_is_decided_by_trade_crossing_not_by_quote() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        h.place(Side::Long, 99.0, 0.5).await;
        h.settle(50).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending]);

        // 盘口整体下移、ask 已跌破挂单价 —— 但不据此成交
        h.bbo(98.0, 98.5).await;
        h.settle(30).await;
        assert!(h.fills().is_empty(), "盘口越价不应成交");

        // 真实成交价严格跌破挂单价 -> 成交，且成交价取挂单价（maker）
        h.trade(98.9).await;
        h.settle(30).await;
        assert_eq!(h.fills(), vec![(99.0, 0.5)]);
        assert!(h.statuses().contains(&OrderStatus::Filled));
    }

    /// 不做部分成交：越价即全量
    #[tokio::test]
    async fn fill_is_always_full_quantity() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        h.place(Side::Long, 99.0, 7.5).await;
        h.settle(50).await;
        // 成交量 1.0 远小于挂单量 7.5，仍全量成交
        h.trade(98.0).await;
        h.settle(30).await;
        assert_eq!(h.fills(), vec![(99.0, 7.5)]);
    }

    /// 延迟期内到达的成交打不到该单上
    #[tokio::test]
    async fn order_does_not_match_before_delay_elapses() {
        let h = Harness::new(300).await;
        h.bbo(100.0, 100.1).await;
        h.place(Side::Long, 99.0, 0.5).await;

        // 延迟未到：单子还没进簿，越价成交也不该成交
        h.settle(60).await;
        h.trade(98.0).await;
        h.settle(30).await;
        assert!(h.statuses().is_empty(), "延迟未到不应有任何回报");
        assert!(h.fills().is_empty(), "延迟未到不应成交");

        // 延迟到期后入簿
        h.settle(300).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending]);
        assert!(h.fills().is_empty(), "入簿后还需新的越价成交才成交");

        h.trade(98.0).await;
        h.settle(30).await;
        assert_eq!(h.fills(), vec![(99.0, 0.5)]);
    }

    /// 延迟到期时若已越过实时盘口 -> 判定为 taker 全量成交（成交价取对手价）
    #[tokio::test]
    async fn order_crossing_live_quote_on_arrival_fills_as_taker() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        // 买价 101 高于卖价 100.1，到达即可成交
        h.place_tif(Side::Long, 101.0, 0.5, TimeInForce::GTC).await;
        h.settle(50).await;
        // 成交价取对手价 100.1（taker），不是挂单价 101
        assert_eq!(h.fills(), vec![(100.1, 0.5)]);
        assert!(h.statuses().contains(&OrderStatus::Filled));
    }

    /// **关键语义**：可成交性在**延迟到期那一刻**按当时的实时盘口判定。
    /// 下单时不可成交，延迟期内盘口涨上来越过挂单价 -> 到达时即 taker 成交。
    #[tokio::test]
    async fn marketability_is_judged_when_delay_elapses_not_at_submit() {
        let h = Harness::new(200).await;
        h.bbo(100.0, 100.1).await;
        // 提交时 100.05 低于卖价 100.1，不可成交
        h.place_tif(Side::Long, 100.05, 0.5, TimeInForce::GTC).await;

        // 延迟期内盘口下移，卖价跌到 100.0 <= 100.05 -> 到达时变为可成交
        h.settle(60).await;
        h.bbo(99.9, 100.0).await;
        assert!(h.statuses().is_empty(), "延迟未到不应有回报");

        h.settle(250).await;
        assert_eq!(
            h.fills(),
            vec![(100.0, 0.5)],
            "应按到达时的实时盘口以 taker 成交于对手价 100.0"
        );
    }

    /// 反向：提交时可成交，但延迟期内盘口走开 -> 到达时改为挂单（不成交）
    #[tokio::test]
    async fn order_rests_if_quote_moves_away_during_delay() {
        let h = Harness::new(200).await;
        h.bbo(100.0, 100.1).await;
        h.place_tif(Side::Long, 100.2, 0.5, TimeInForce::GTC).await; // 提交时越过卖价

        h.settle(60).await;
        h.bbo(100.5, 100.6).await; // 盘口涨走，100.2 不再可成交

        h.settle(250).await;
        assert_eq!(h.statuses(), vec![OrderStatus::Pending], "应改为挂单");
        assert!(h.fills().is_empty());
    }

    /// taker 成交按 taker 费率计费（与 maker 不同）
    #[tokio::test]
    async fn taker_fill_charges_taker_fee() {
        let pubsub = PaperPubSub::spawn_with_mailbox(
            PaperPubSub::new(DeliveryStrategy::Guaranteed),
            mailbox::unbounded(),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink::spawn_with_mailbox(events.clone(), mailbox::unbounded());
        pubsub.tell(Subscribe(sink)).send().await.unwrap();
        let counter = PaperCounterActor::spawn_with_mailbox(
            PaperCounterArgs {
                paper_pubsub: pubsub,
                config: SimConfig {
                    initial_balance_usdt: 10_000.0,
                    maker_fee_rate: 0.0002,
                    taker_fee_rate: 0.0005,
                    order_to_exchange_delay_ms: 0,
                    exchange_to_strategy_delay_ms: 0,
                },
            },
            mailbox::unbounded(),
        );
        let h = Harness { counter, events };
        h.bbo(100.0, 100.1).await;
        h.place_tif(Side::Long, 101.0, 2.0, TimeInForce::GTC).await;
        h.settle(50).await;

        let fee = h
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match &e.data {
                ExchangeEventData::Fill(f) => Some(f.fee),
                _ => None,
            })
            .expect("fill");
        // 100.1 * 2 * 0.0005 = 0.1001（若按 maker 费率会是 0.04004）
        assert!((fee - 0.1001).abs() < 1e-9, "taker 费率应生效，实得 {fee}");
    }

    /// 撤单同样经过延迟；撤掉后不再成交
    #[tokio::test]
    async fn cancel_removes_order_from_book() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        h.place(Side::Long, 99.0, 0.5).await;
        h.settle(50).await;
        let order_id = h.order_ids().first().cloned().expect("order id");

        h.counter
            .tell(AccountOutcome {
                account: account(),
                event: OutcomeEvent::CancelOrder {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    order_id,
                    client_order_id: "c1".to_string(),
                },
            })
            .send()
            .await
            .unwrap();
        h.settle(50).await;
        assert!(h.statuses().contains(&OrderStatus::Cancelled));

        h.trade(98.0).await;
        h.settle(30).await;
        assert!(h.fills().is_empty(), "已撤单不应成交");
    }

    /// PostOnly 到达时若已可成交 -> 拒单（与真实交易所一致）
    #[tokio::test]
    async fn marketable_post_only_is_rejected() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        h.place(Side::Long, 100.5, 0.5).await; // 高于 ask，会吃单
        h.settle(50).await;
        assert!(
            h.statuses()
                .iter()
                .any(|s| matches!(s, OrderStatus::Rejected { .. })),
            "得到 {:?}",
            h.statuses()
        );
        assert!(h.fills().is_empty());
    }

    /// 行情回显绝不能被再次发布 —— 柜台自己也订阅 income，回显会导致无限循环
    #[tokio::test]
    async fn market_events_are_never_republished() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        h.trade(100.0).await;
        // 标记价同样经 on_market（用于估值），其回显也不得外发
        h.counter
            .tell(IncomeEvent {
                exchange_ts: 3,
                local_ts: 3,
                data: ExchangeEventData::MarkPrice(crate::domain::MarkPrice {
                    exchange: EX,
                    symbol: SYM.to_string(),
                    price: 100.0,
                    timestamp: 3,
                }),
            })
            .send()
            .await
            .unwrap();
        h.settle(50).await;

        let republished: Vec<&'static str> = h
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.data {
                ExchangeEventData::BBO(_) => Some("BBO"),
                ExchangeEventData::MarketTrade(_) => Some("MarketTrade"),
                ExchangeEventData::MarkPrice(_) => Some("MarkPrice"),
                _ => None,
            })
            .collect();
        assert!(
            republished.is_empty(),
            "柜台把行情回显发回了总线（会与自身订阅形成无限循环）: {republished:?}"
        );
    }

    /// Clock 按账户发布净值（策略依赖 equity 才能动作）。
    ///
    /// 账户是惰性创建的：先下单开出账户，Clock 才有账户可报 —— 没有账户就没有净值可言。
    #[tokio::test]
    async fn clock_publishes_account_info_per_account() {
        let h = Harness::new(0).await;
        h.bbo(100.0, 100.1).await;
        h.place(Side::Long, 99.0, 0.5).await;
        h.settle(50).await;
        h.counter
            .tell(IncomeEvent {
                exchange_ts: 5,
                local_ts: 5,
                data: ExchangeEventData::Clock,
            })
            .send()
            .await
            .unwrap();
        h.settle(30).await;
        let equity = h.events.lock().unwrap().iter().find_map(|e| match &e.data {
            ExchangeEventData::AccountInfo { equity, .. } => Some(*equity),
            _ => None,
        });
        assert_eq!(equity, Some(10_000.0));
    }
}

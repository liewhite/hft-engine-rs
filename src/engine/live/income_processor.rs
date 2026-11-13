//! IncomeProcessor —— 入向事件分发
//!
//! 订阅 MarketBus 与 AccountBus（全量），按订阅范围分发到对应的 [`Executor`](super::Executor)。
//! "这条事件归不归它"一律问 [`SubscriptionScope::accepts`]（账户归属、所、symbol 都在那里判），
//! 这里只管扇出：
//! - 行情（[`MarketEvent`]）：一份服务所有账户 —— 有 symbol 的经 `(所, symbol)` 索引定向，
//!   其余逐个问范围；
//! - 账户事件（[`AccountEvent`]）：经账户索引只看该账户的 executor，再问范围。
//!
//! # 路由索引
//!
//! 分发是热路径（每条 BBO 都经过），按 (exchange, symbol) 与账户建反向索引做 O(1)
//! 查找 —— 此前对每条事件线性扫描全部 executor，数百 symbol 的部署下每个行情 tick
//! 都是 O(n) 扫描。
//!
//! # 名单由编排维护
//!
//! 投产时 [`IncomeProcessorHandle::register`]（ask，返回即已生效）。实例不单独撤下；停机途中
//! 已停止收件的实例在投递失败时摘出名单（见 [`IncomeProcessor::drop_gone`]）。

use super::executor::{ExecutorHandle, ExecutorId};
use crate::domain::{AccountId, Exchange, Symbol};
use crate::messaging::{
    AccountBus, AccountEvent, Delivery, IncomeEvent, MarketBus, MarketEvent, SubscriptionScope,
};
use crate::tasks::{Addr, ChildHandle, Gone, Group, Inbox, Scope, Task};
use std::collections::HashMap;
use tokio::sync::oneshot;

/// Executor 订阅信息
struct ExecutorSubscription {
    executor: ExecutorHandle,
    /// 订阅范围（含绑定账户）。"这条事件归不归它"一律问 [`SubscriptionScope::accepts`]，
    /// 与回测驱动同一份判据 —— 下面的两个反向索引只是热路径的扇出优化。
    scope: SubscriptionScope,
}

/// IncomeProcessor 的收件箱消息
pub enum IncomeProcessorMsg {
    /// 行情总线（全量订阅）
    Market(MarketEvent),
    /// 账户总线（全量订阅）
    Account(AccountEvent),
    /// 见 [`IncomeProcessorHandle::register`]
    Register {
        executor: ExecutorHandle,
        scope: SubscriptionScope,
        reply: oneshot::Sender<()>,
    },
}

/// IncomeProcessor 的地址（可克隆）
#[derive(Clone, Debug)]
pub struct IncomeProcessorHandle(Addr<IncomeProcessorMsg>);

impl IncomeProcessorHandle {
    /// 注册 executor 及其订阅范围（与它的 runner 是同一份，投产时算一次）。返回即已生效：
    /// 此后的事件按范围投给它。每个 executor 只注册一次（投产只有一次，`ExecutorId` 唯一）。
    pub async fn register(
        &self,
        executor: ExecutorHandle,
        scope: SubscriptionScope,
    ) -> Result<(), Gone> {
        self.0
            .ask(|reply| IncomeProcessorMsg::Register {
                executor,
                scope,
                reply,
            })
            .await
    }
}

/// 入向事件分发
pub struct IncomeProcessor {
    inbox: Inbox<IncomeProcessorMsg>,
    executors: HashMap<ExecutorId, ExecutorSubscription>,
    /// 反向索引：(exchange, symbol) -> 订阅者（热路径 O(1) 查找）
    by_symbol: HashMap<(Exchange, Symbol), Vec<ExecutorId>>,
    /// 反向索引：账户 -> 该账户的全部 executor
    by_account: HashMap<AccountId, Vec<ExecutorId>>,
}

impl IncomeProcessor {
    /// 在 `group`（consumers）下起分发器，构造时订阅两条入向总线的全量事件。
    pub async fn spawn(
        group: &Group,
        market_bus: &MarketBus,
        account_bus: &AccountBus,
    ) -> anyhow::Result<(ChildHandle, IncomeProcessorHandle)> {
        let (market_bus, account_bus) = (market_bus.clone(), account_bus.clone());
        let (child, addr) = group
            .spawn_addressed(
                "IncomeProcessor",
                move |_scope: Scope, mut inbox| async move {
                    inbox.subscribe(&market_bus, |_| true, IncomeProcessorMsg::Market);
                    inbox.subscribe(&account_bus, |_| true, IncomeProcessorMsg::Account);
                    tracing::info!("IncomeProcessor started");
                    Ok(Self {
                        inbox,
                        executors: HashMap::new(),
                        by_symbol: HashMap::new(),
                        by_account: HashMap::new(),
                    })
                },
            )
            .await?;
        Ok((child, IncomeProcessorHandle(addr)))
    }

    fn handle(&mut self, msg: IncomeProcessorMsg) {
        // 回执发送失败 = 提问方已不再等，没有人需要这份确认
        match msg {
            IncomeProcessorMsg::Market(event) => self.on_market(event),
            IncomeProcessorMsg::Account(event) => self.on_account(event),
            IncomeProcessorMsg::Register {
                executor,
                scope,
                reply,
            } => {
                self.register(executor, scope);
                let _ = reply.send(());
            }
        }
    }

    fn register(&mut self, executor: ExecutorHandle, scope: SubscriptionScope) {
        let id = executor.id();
        tracing::info!(executor = %id, scope = ?scope, "Executor registered");

        for (exchange, symbol) in scope.pairs() {
            self.by_symbol
                .entry((exchange, symbol.clone()))
                .or_default()
                .push(id);
        }
        self.by_account
            .entry(scope.account().clone())
            .or_default()
            .push(id);
        self.executors
            .insert(id, ExecutorSubscription { executor, scope });
    }

    fn remove_from_indexes(&mut self, id: ExecutorId) {
        if let Some(sub) = self.executors.remove(&id) {
            for (exchange, symbol) in sub.scope.pairs() {
                let key = (exchange, symbol.clone());
                if let Some(v) = self.by_symbol.get_mut(&key) {
                    v.retain(|x| *x != id);
                    if v.is_empty() {
                        self.by_symbol.remove(&key);
                    }
                }
            }
            if let Some(v) = self.by_account.get_mut(sub.scope.account()) {
                v.retain(|x| *x != id);
                if v.is_empty() {
                    self.by_account.remove(sub.scope.account());
                }
            }
        }
    }

    /// 行情事件：一份服务所有账户。有 symbol 的定向投给订阅者，无 symbol 的按范围投。
    fn on_market(&mut self, msg: MarketEvent) {
        let event = IncomeEvent::Market(msg);
        let mut gone = Vec::new();
        match event.delivery() {
            Delivery::Symbol(exchange, symbol) => {
                // 定向：索引 O(1)。索引的键由 `SubscriptionScope::pairs()` 灌，与判据同源，
                // 所以这里不再问一遍 accepts —— 但那是**结构约定**，不是编译期保证，
                // 故留一条 debug 断言：索引一旦与范围失配，回测与测试里会立刻响。
                if let Some(ids) = self.by_symbol.get(&(exchange, symbol.clone())) {
                    for id in ids {
                        if let Some(sub) = self.executors.get(id) {
                            debug_assert!(
                                sub.scope.accepts(&event),
                                "by_symbol 索引与订阅范围失配：事件被投给了范围外的 executor"
                            );
                            Self::forward(sub, &event, &mut gone);
                        }
                    }
                }
            }
            // 所级公共读数（ExchangeStatus / ExchangeRate）只投给订了该所的；
            // 与交易所无关的全局事件（Clock / 无 scope 的 Custom）广播。
            // 两档都由同一个判据回答，不在这里重写一遍。
            Delivery::Exchange(_) | Delivery::Broadcast => {
                for sub in self.executors.values() {
                    if sub.scope.accepts(&event) {
                        Self::forward(sub, &event, &mut gone);
                    }
                }
            }
        }
        self.drop_gone(gone);
    }

    /// 账户事件：经账户索引只看**该账户**的 executor，再按订阅范围过滤。
    fn on_account(&mut self, msg: AccountEvent) {
        let account = msg.account.clone();
        let event = IncomeEvent::Account(msg);
        let Some(ids) = self.by_account.get(&account) else {
            return;
        };
        let mut gone = Vec::new();
        for id in ids {
            let Some(sub) = self.executors.get(id) else {
                continue;
            };
            if !sub.scope.accepts(&event) {
                // 该账户的私有事件、但不在这个 executor 的订阅范围内：分桶部署下实盘账户级全量私有流
                // 含其他桶的 symbol，多个实例共用一个模拟账户时是别的实例的回报，都是常态。策略只能在
                // 订阅范围内下单（下单闸门 `StrategyRunner::admit`），自己的回报不会在这里被丢
                continue;
            }
            Self::forward(sub, &event, &mut gone);
        }
        self.drop_gone(gone);
    }

    /// 投给一个 executor；对方已停止收件则记进 `gone`（由 [`Self::drop_gone`] 处理）。
    fn forward(sub: &ExecutorSubscription, event: &IncomeEvent, gone: &mut Vec<ExecutorId>) {
        if sub.executor.deliver(event.clone()).is_err() {
            gone.push(sub.executor.id());
        }
    }

    /// 投递失败（`Gone`）= 该 executor 已停止收件。分发层是一对多分发方：记 warn 并把它摘出
    /// 名单（之后不会再为它报第二次），分发器照常运行。来路与理由见 `docs/architecture.md` 3.1
    /// 「Gone 的来路与处理」。
    fn drop_gone(&mut self, gone: Vec<ExecutorId>) {
        for id in gone {
            tracing::warn!(executor = %id, "executor 已停止收件，事件未送达，移出分发名单（停机途中属正常）");
            self.remove_from_indexes(id);
        }
    }
}

impl Task for IncomeProcessor {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        while let Some(msg) = self.inbox.recv().await {
            self.handle(msg);
        }
        tracing::info!("IncomeProcessor stopped");
        Ok(())
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::domain::{Exchange, Fill, FillReason, MarketTrade, Side};
    use crate::messaging::SymbolMarketData;
    use crate::messaging::{AccountData, MarketData, SymbolAccountData};
    use rust_decimal_macros::dec;

    fn fill_event(account: AccountId) -> AccountEvent {
        AccountEvent {
            account,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Symbol(SymbolAccountData::Fill(Fill {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                side: Side::Long,
                price: dec!(100.0),
                size: dec!(1.0),
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: 1,
                fee: 0.0,
                reason: FillReason::Normal,
            })),
        }
    }

    fn trade_event() -> MarketEvent {
        MarketEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: MarketData::Symbol(SymbolMarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                price: dec!(100.0),
                qty: dec!(1.0),
                is_buyer_maker: false,
                timestamp: 1,
            })),
        }
    }

    /// 账户隔离的核心：账户事件的归属是**结构字段**，路由按它分道 —— "实盘私有事件
    /// 广播给模拟策略"在类型上已不可表达，这里钉住路由推导本身。
    #[test]
    fn account_event_carries_its_owner_and_routes_by_symbol() {
        let live = fill_event(AccountId::Live);
        assert_eq!(live.account, AccountId::Live);
        let ev = IncomeEvent::Account(live);
        assert_eq!(
            ev.delivery(),
            Delivery::Symbol(Exchange::Binance, &"BTC".to_string()),
            "Fill 有 symbol，应定向路由"
        );
    }

    /// 三档投递范围各取一个代表，钉住分发层与 `accepts` 共用的这份推导。
    ///
    /// 尤其是中间那档：**所级账户读数按所定向，不是广播**。写成广播的那版里，
    /// 策略能读到自己压根没订的所的净值，而杠杆闸门就是拿净值算的。
    #[test]
    fn delivery_scope_matches_the_events_locating_precision() {
        let ev = IncomeEvent::Market(trade_event());
        assert_eq!(
            ev.delivery(),
            Delivery::Symbol(Exchange::Binance, &"BTC".to_string()),
            "有 symbol 的行情按 (所, symbol) 定向"
        );

        let account_info = IncomeEvent::account(
            AccountId::Live,
            0,
            0,
            crate::messaging::AccountData::Exchange(
                crate::messaging::ExchangeAccountData::AccountInfo {
                    exchange: Exchange::Binance,
                    info: crate::domain::AccountInfo {
                        equity: 1.0,
                        notional: 0.0,
                    },
                },
            ),
        );
        assert_eq!(
            account_info.delivery(),
            Delivery::Exchange(Exchange::Binance),
            "所级账户读数按所定向 —— 广播会把净值泄漏给没订这个所的策略"
        );

        let clock = IncomeEvent::market(0, 0, MarketData::Clock);
        assert_eq!(
            clock.delivery(),
            Delivery::Broadcast,
            "Clock 与交易所无关，广播"
        );
    }
}

/// 组件层：注册后按范围投递、对方停止收件不打死分发器。用 `tasks::Root` 搭树测。
#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::domain::{Fill, FillReason, MarketTrade, Side};
    use crate::engine::live::executor::ExecutorMsg;
    use crate::messaging::SymbolMarketData;
    use crate::messaging::{AccountData, MarketData, SymbolAccountData};
    use crate::tasks::Root;
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// 实盘账户、只订 Binance BTC 的范围
    fn btc_scope() -> SubscriptionScope {
        SubscriptionScope::new(AccountId::Live, [(Exchange::Binance, "BTC".to_string())])
    }

    fn trade(symbol: &str) -> MarketEvent {
        MarketEvent {
            exchange_ts: 1,
            local_ts: 1,
            data: MarketData::Symbol(SymbolMarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: symbol.to_string(),
                price: dec!(100.0),
                qty: dec!(1.0),
                is_buyer_maker: false,
                timestamp: 1,
            })),
        }
    }

    fn live_fill() -> AccountEvent {
        AccountEvent {
            account: AccountId::Live,
            exchange_ts: 1,
            local_ts: 1,
            data: AccountData::Symbol(SymbolAccountData::Fill(Fill {
                exchange: Exchange::Binance,
                symbol: "BTC".to_string(),
                side: Side::Long,
                price: dec!(100.0),
                size: dec!(1.0),
                client_order_id: None,
                order_id: "1".to_string(),
                timestamp: 1,
                fee: 0.0,
                reason: FillReason::Normal,
            })),
        }
    }

    /// 代替 executor 的收件端：记下收到的事件条数
    async fn fake_executor(root: &Root) -> (ChildHandle, ExecutorHandle, Arc<Mutex<usize>>) {
        let got = Arc::new(Mutex::new(0usize));
        let (addr_tx, addr_rx) = oneshot::channel();
        let sink = got.clone();
        let child = root
            .group("producers")
            .spawn_fn("fake-executor", move |scope| async move {
                let (addr, mut inbox) = scope.inbox::<ExecutorMsg>();
                addr_tx.send(addr).unwrap();
                while let Some(msg) = inbox.recv().await {
                    if let ExecutorMsg::Event(_) = msg {
                        *sink.lock().unwrap() += 1;
                    }
                }
                Ok(())
            })
            .unwrap();
        (child, ExecutorHandle::for_test(addr_rx.await.unwrap()), got)
    }

    /// 让分发器处理完之前进箱的事件（同一收件箱 FIFO：一次 ask 应答 = 前面的都已处理；
    /// 拿注册一个什么都不订的实例当这次 ask）
    async fn settle(root: &Root, p: &IncomeProcessorHandle) {
        let (_, barrier, _) = fake_executor(root).await;
        let nothing = SubscriptionScope::new(AccountId::Live, std::iter::empty());
        p.register(barrier, nothing).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test]
    async fn routes_by_scope() {
        let root = Root::new("t");
        let (market, account) = (MarketBus::new("market"), AccountBus::new("account"));
        let (_c, processor) = IncomeProcessor::spawn(&root.group("consumers"), &market, &account)
            .await
            .unwrap();
        let (_e, executor, got) = fake_executor(&root).await;
        processor.register(executor, btc_scope()).await.unwrap();

        market.publish(trade("BTC"));
        market.publish(trade("ETH")); // 范围外
        account.publish(live_fill());
        settle(&root, &processor).await;
        assert_eq!(
            *got.lock().unwrap(),
            2,
            "BTC 行情与该账户的 BTC 成交各一条，ETH 不投"
        );
        root.shutdown().await.unwrap();
    }

    /// executor 已停止收件（停机途中的常态）：分发器记 warn、摘掉它，自己照常运行
    #[tokio::test]
    async fn a_gone_executor_never_halts_the_processor() {
        let root = Root::new("t");
        let (market, account) = (MarketBus::new("market"), AccountBus::new("account"));
        let (_c, processor) = IncomeProcessor::spawn(&root.group("consumers"), &market, &account)
            .await
            .unwrap();
        let (child, executor, _got) = fake_executor(&root).await;
        processor.register(executor, btc_scope()).await.unwrap();
        child.stop().await;

        market.publish(trade("BTC"));
        market.publish(trade("BTC"));
        settle(&root, &processor).await;
        assert!(!root.is_stopping(), "executor 停止收件不是分发层的事故");
        root.shutdown().await.unwrap();
    }
}
